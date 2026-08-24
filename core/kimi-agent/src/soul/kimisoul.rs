use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tempfile::tempdir;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use kosong::chat_provider::{ChatProviderError, ChatProviderErrorKind};
use kosong::message::{ContentPart, Message, Role, StreamedMessagePart, TextPart};
use kosong::{StepResult, step as kosong_step};

use crate::config::{ModelCapability, save_config};
use crate::llm::create_llm;
use crate::skill::flow::{Flow, FlowEdge, FlowLabel, FlowNode, FlowNodeKind, parse_choice};
use crate::skill::{Skill, SkillType, read_skill_text};
use crate::soul::agent::{Agent, Runtime};
use crate::soul::{
    LLMNotSet, LLMNotSupported, MaxStepsReached, Soul, StatusSnapshot,
    approval::Approval,
    compaction::{Compaction, SimpleCompaction},
    context::Context,
    message::{check_message, system, tool_result_to_message},
    state::write_state,
    wire_send,
};
use crate::tools::fork::{ForkParams, ForkTool};
use crate::tools::utils::is_tool_rejected;
use crate::utils::{SlashCommandInfo, parse_slash_command_call};
use crate::wire::{
    ApprovalRequest, ApprovalResponse, CompactionBegin, CompactionEnd, StatusUpdate, SteerInput,
    StepBegin, StepInterrupted, TurnBegin, TurnEnd, UserInput, WireMessage,
};

use kosong::tooling::{CallableTool2, ToolOutput, Toolset};

const SKILL_COMMAND_PREFIX: &str = "skill:";
const FLOW_COMMAND_PREFIX: &str = "flow:";
const DEFAULT_MAX_FLOW_MOVES: i64 = 1000;

type StepStopReason = &'static str;

pub struct StepOutcome {
    pub stop_reason: StepStopReason,
    pub assistant_message: Message,
}

pub struct TurnOutcome {
    pub stop_reason: StepStopReason,
    pub final_message: Option<Message>,
    pub step_count: i64,
}

#[derive(Clone, Debug, Error)]
#[error("back to the future")]
struct BackToTheFuture {
    checkpoint_id: i64,
    messages: Vec<Message>,
}

pub struct KimiSoul {
    agent: Agent,
    runtime: Runtime,
    context: tokio::sync::Mutex<Context>,
    compaction: SimpleCompaction,
    checkpoint_with_user_message: bool,
    slash_commands: Vec<SlashCommandInfo>,
    slash_handlers: HashMap<String, SlashHandler>,
    cached_model_name: std::sync::Mutex<String>,
    /// User inputs injected mid-turn via `steer`, consumed between steps.
    steer_queue: std::sync::Mutex<std::collections::VecDeque<UserInput>>,
    /// Task IDs of background forks the user spawned via `/fork`. Drained at the
    /// start of each turn: completed ones get their result injected into context
    /// so the agent (and user) see it on the next interaction.
    pending_fork_tasks: std::sync::Mutex<Vec<String>>,
}

enum SlashHandler {
    Builtin(BuiltinSlash),
    Skill(Skill),
    Flow(FlowRunner),
}

#[derive(Clone, Copy)]
enum BuiltinSlash {
    Init,
    Compact,
    Clear,
    Yolo,
    Model,
    Fork,
}

fn parse_model_args(args: &str) -> (&str, bool) {
    let mut thinking = true; // default on
    let mut model_name = args;

    if let Some(idx) = args.find(" --thinking") {
        let rest = &args[idx..];
        model_name = args[..idx].trim();
        if rest.contains("off") || rest.contains("false") {
            thinking = false;
        } else if rest.contains("on") || rest.contains("true") {
            thinking = true;
        }
    }

    (model_name, thinking)
}

impl KimiSoul {
    pub fn new(agent: Agent, context: Context) -> Self {
        let checkpoint_with_user_message = agent
            .toolset
            .try_lock()
            .map(|guard| guard.tools().iter().any(|tool| tool.name == "SendDMail"))
            .unwrap_or(false);

        let cached_model_name = std::sync::Mutex::new(
            agent
                .runtime
                .llm
                .try_read()
                .ok()
                .and_then(|guard| guard.as_ref().map(|llm| llm.model_name().to_string()))
                .unwrap_or_default(),
        );

        let mut soul = KimiSoul {
            runtime: agent.runtime.clone(),
            agent,
            context: tokio::sync::Mutex::new(context),
            compaction: SimpleCompaction::new(2),
            checkpoint_with_user_message,
            slash_commands: Vec::new(),
            slash_handlers: HashMap::new(),
            cached_model_name,
            steer_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            pending_fork_tasks: std::sync::Mutex::new(Vec::new()),
        };
        soul.build_slash_commands();
        soul
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn context(&self) -> &tokio::sync::Mutex<Context> {
        &self.context
    }

    /// Queue a user input for injection into the running turn (Wire `steer`).
    /// Consumed between steps; emits a `SteerInput` event when applied.
    pub fn steer(&self, user_input: UserInput) {
        self.steer_queue
            .lock()
            .expect("steer_queue poisoned")
            .push_back(user_input);
    }

    fn clear_pending_steers(&self) {
        self.steer_queue
            .lock()
            .expect("steer_queue poisoned")
            .clear();
    }

    /// Drain queued steers, append each to context as a user message, and emit a
    /// `SteerInput` event. Returns true if any were injected.
    async fn consume_pending_steers(&self) -> Result<bool, anyhow::Error> {
        let pending: Vec<UserInput> = {
            let mut queue = self.steer_queue.lock().expect("steer_queue poisoned");
            queue.drain(..).collect()
        };
        if pending.is_empty() {
            return Ok(false);
        }
        for user_input in pending {
            let user_message = match user_input.clone() {
                UserInput::Text(text) => {
                    Message::new(Role::User, vec![ContentPart::Text(TextPart::new(text))])
                }
                UserInput::Parts(parts) => Message::new(Role::User, parts),
            };
            {
                let mut context = self.context.lock().await;
                context.append_messages(user_message).await?;
            }
            wire_send(WireMessage::SteerInput(SteerInput { user_input }));
        }
        Ok(true)
    }

    fn build_slash_commands(&mut self) {
        let mut commands = Vec::new();
        let mut handlers = HashMap::new();

        let builtin = vec![
            (
                "init",
                "Analyze the codebase and generate an `AGENTS.md` file",
                BuiltinSlash::Init,
                vec![],
            ),
            (
                "compact",
                "Compact the context",
                BuiltinSlash::Compact,
                vec![],
            ),
            (
                "clear",
                "Clear the context",
                BuiltinSlash::Clear,
                vec!["reset"],
            ),
            (
                "yolo",
                "Toggle YOLO mode (auto-approve all actions)",
                BuiltinSlash::Yolo,
                vec![],
            ),
            (
                "model",
                "Switch the LLM model (e.g. /model deepseek/deepseek-v4-pro --thinking on)",
                BuiltinSlash::Model,
                vec![],
            ),
            (
                "fork",
                "Spawn a background fork that shares this conversation (e.g. /fork investigate the failing test)",
                BuiltinSlash::Fork,
                vec![],
            ),
        ];

        for (name, description, kind, aliases) in builtin {
            commands.push(SlashCommandInfo {
                name: name.to_string(),
                description: description.to_string(),
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
            });
            handlers.insert(name.to_string(), SlashHandler::Builtin(kind));
            for alias in aliases {
                handlers.insert(alias.to_string(), SlashHandler::Builtin(kind));
            }
        }

        let mut seen = handlers
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut skills: Vec<_> = self.runtime.skills.values().cloned().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        for skill in &skills {
            if skill.skill_type != SkillType::Standard && skill.skill_type != SkillType::Flow {
                continue;
            }
            let name = format!("{SKILL_COMMAND_PREFIX}{}", skill.name);
            if seen.contains(&name) {
                warn!(
                    "Skipping skill slash command /{}: name already registered",
                    name
                );
                continue;
            }
            commands.push(SlashCommandInfo {
                name: name.clone(),
                description: skill.description.clone(),
                aliases: Vec::new(),
            });
            handlers.insert(name.clone(), SlashHandler::Skill(skill.clone()));
            seen.insert(name);
        }

        for skill in &skills {
            if skill.skill_type != SkillType::Flow {
                continue;
            }
            if skill.flow.is_none() {
                warn!("Flow skill {} has no flow; skipping", skill.name);
                continue;
            }
            let name = format!("{FLOW_COMMAND_PREFIX}{}", skill.name);
            if seen.contains(&name) {
                warn!(
                    "Skipping prompt flow slash command /{}: name already registered",
                    name
                );
                continue;
            }
            let runner = FlowRunner::new(
                skill.flow.clone().unwrap(),
                Some(skill.name.clone()),
                DEFAULT_MAX_FLOW_MOVES,
            );
            commands.push(SlashCommandInfo {
                name: name.clone(),
                description: skill.description.clone(),
                aliases: Vec::new(),
            });
            handlers.insert(name.clone(), SlashHandler::Flow(runner));
            seen.insert(name);
        }

        self.slash_commands = commands;
        self.slash_handlers = handlers;
    }

    async fn checkpoint(&self) -> anyhow::Result<()> {
        let mut context = self.context.lock().await;
        context
            .checkpoint(self.checkpoint_with_user_message)
            .await?;
        Ok(())
    }

    async fn handle_slash(&self, name: &str, args: &str) -> anyhow::Result<()> {
        match self.slash_handlers.get(name) {
            Some(SlashHandler::Builtin(kind)) => match kind {
                BuiltinSlash::Init => self.slash_init().await,
                BuiltinSlash::Compact => self.slash_compact().await,
                BuiltinSlash::Clear => self.slash_clear().await,
                BuiltinSlash::Yolo => self.slash_yolo().await,
                BuiltinSlash::Model => self.slash_model(args).await,
                BuiltinSlash::Fork => self.slash_fork(args).await,
            },
            Some(SlashHandler::Skill(skill)) => self.run_skill(skill, args).await,
            Some(SlashHandler::Flow(runner)) => runner.run(self, args).await,
            None => {
                wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                    format!("Unknown slash command \"/{}\".", name),
                ))));
                Ok(())
            }
        }
    }

    async fn slash_init(&self) -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let tmp_path = temp_dir.path().join("context.jsonl");
        let tmp_context = Context::new(tmp_path);
        let tmp_soul = KimiSoul::new(self.agent.clone(), tmp_context);
        tmp_soul
            .run(UserInput::Text(crate::prompts::INIT.to_string()))
            .await?;

        let agents_md =
            crate::soul::agent::load_agents_md(&self.runtime.builtin_args.KIMI_WORK_DIR)
                .await
                .unwrap_or_default();
        let system_message = system(&format!(
            "The user just ran `/init` slash command. The system has analyzed the codebase and generated an `AGENTS.md` file. Latest AGENTS.md file content:\n{}",
            agents_md
        ));
        let mut context = self.context.lock().await;
        context
            .append_messages(Message::new(Role::User, vec![system_message]))
            .await?;
        Ok(())
    }

    async fn slash_compact(&self) -> anyhow::Result<()> {
        info!("Running `/compact`");
        let context = self.context.lock().await;
        if context.n_checkpoints() == 0 {
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                "The context is empty.",
            ))));
            return Ok(());
        }
        drop(context);
        self.compact_context().await?;
        wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
            "The context has been compacted.",
        ))));
        self.send_status_update();
        Ok(())
    }

    async fn slash_clear(&self) -> anyhow::Result<()> {
        info!("Running `/clear`");
        {
            let mut context = self.context.lock().await;
            context.clear().await?;
        }
        wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
            "The context has been cleared.",
        ))));
        self.send_status_update();
        Ok(())
    }

    pub fn current_status_update(&self) -> StatusUpdate {
        let (context_usage, context_tokens, max_context_tokens) =
            if let Ok(guard) = self.runtime.llm.try_read() {
                if let Some(llm) = guard.as_ref() {
                    match self.context.try_lock() {
                        Ok(context) => {
                            let n = context.token_count();
                            // Send None for context_tokens when count is 0 so the Python
                            // merge logic retains the previously-displayed value (e.g. from
                            // wire replay) rather than zeroing it out.
                            let tokens = if n > 0 { Some(n as i64) } else { None };
                            (
                                n as f64 / llm.max_context_size as f64,
                                tokens,
                                Some(llm.max_context_size),
                            )
                        }
                        Err(_) => (0.0, None, None),
                    }
                } else {
                    (0.0, None, None)
                }
            } else {
                (0.0, None, None)
            };
        StatusUpdate {
            context_usage: Some(context_usage),
            context_tokens,
            max_context_tokens,
            token_usage: None,
            message_id: None,
            model: Some(self.cached_model_name.lock().unwrap().clone()),
            yolo_enabled: Some(self.runtime.approval.is_yolo()),
            thinking: self.thinking(),
        }
    }

    fn send_status_update(&self) {
        wire_send(WireMessage::StatusUpdate(self.current_status_update()));
    }

    async fn slash_yolo(&self) -> anyhow::Result<()> {
        if self.runtime.approval.is_yolo() {
            self.runtime.approval.set_yolo(false);
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                "You only die once! Actions will require approval.",
            ))));
        } else {
            self.runtime.approval.set_yolo(true);
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                "You only live once! All actions will be auto-approved.",
            ))));
        }
        self.send_status_update();
        write_state(&self.runtime.session, &self.runtime).await;
        Ok(())
    }

    async fn slash_model(&self, args: &str) -> anyhow::Result<()> {
        let args = args.trim();
        if args.is_empty() {
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                format!(
                    "Current model: {} (thinking: {})",
                    self.runtime.config.default_model,
                    if self.runtime.config.default_thinking {
                        "on"
                    } else {
                        "off"
                    }
                ),
            ))));
            self.send_status_update();
            return Ok(());
        }

        let (model_name, thinking) = parse_model_args(args);

        let model = self
            .runtime
            .config
            .models
            .get(model_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown model \"{}\"", model_name))?;
        let provider = self
            .runtime
            .config
            .providers
            .get(&model.provider)
            .ok_or_else(|| {
                anyhow::anyhow!("Provider \"{}\" not found for model", model.provider)
            })?;

        let new_llm = create_llm(
            provider,
            model,
            Some(thinking),
            Some(&self.runtime.session.id),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create LLM: {}", e))?;

        let new_llm = new_llm.map(Arc::new);
        let display_name = new_llm
            .as_ref()
            .map(|llm| llm.model_name().to_string())
            .unwrap_or_else(|| model_name.to_string());

        *self.runtime.llm.write().await = new_llm;
        *self.cached_model_name.lock().unwrap() = display_name.clone();
        {
            let mut config = self.runtime.config.clone();
            config.default_model = model_name.to_string();
            config.default_thinking = thinking;
            save_config(&config, None).await?;
        }
        self.send_status_update();
        wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
            format!(
                "Switched to {} (thinking: {})",
                display_name,
                if thinking { "on" } else { "off" }
            ),
        ))));
        Ok(())
    }

    /// User-facing `/fork <prompt>`: spawn a background fork that shares this
    /// conversation's context. Mirrors the `Fork` tool the agent can call, so
    /// the user can launch one just as easily.
    async fn slash_fork(&self, args: &str) -> anyhow::Result<()> {
        let prompt = args.trim();
        if prompt.is_empty() {
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                "Usage: /fork <prompt> — spawns a background fork that shares this conversation's \
                 context and works on <prompt>. You'll be notified when it finishes.",
            ))));
            return Ok(());
        }

        let tool = ForkTool::new(&self.runtime);
        let result = tool
            .call_typed(ForkParams {
                prompt: prompt.to_string(),
                description: String::new(),
            })
            .await;

        if result.is_error {
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                format!("Fork failed: {}", result.message),
            ))));
            return Ok(());
        }

        let output = match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => String::new(),
        };
        let task_id = output
            .lines()
            .find_map(|line| line.strip_prefix("Task ID:"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Record the fork in the parent context so the agent knows the user
        // spawned it. Its result is delivered automatically at the next turn
        // once it finishes (see surface_completed_forks). Without this the fork
        // would be invisible to the agent.
        let note = if task_id.is_empty() {
            format!(
                "The user spawned a background fork (a concurrent copy of you sharing this \
                 conversation) with the prompt: \"{prompt}\". It runs independently; its result \
                 will be delivered to you automatically once it finishes."
            )
        } else {
            format!(
                "The user spawned a background fork (a concurrent copy of you sharing this \
                 conversation), task_id={task_id}, with the prompt: \"{prompt}\". It runs \
                 independently; its result will be delivered to you automatically once it \
                 finishes."
            )
        };
        {
            let mut context = self.context.lock().await;
            context
                .append_messages(Message::new(Role::User, vec![system(&note)]))
                .await?;
        }
        if !task_id.is_empty() {
            self.pending_fork_tasks
                .lock()
                .expect("pending_fork_tasks poisoned")
                .push(task_id.clone());
        }

        let message = if task_id.is_empty() {
            "Forked into a background task. Its result will surface on your next message."
                .to_string()
        } else {
            format!(
                "Forked into a background task (task_id={task_id}). Its result will surface on \
                 your next message."
            )
        };
        wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
            message,
        ))));
        Ok(())
    }

    /// Drain any `/fork`-spawned background tasks that have finished, injecting
    /// their output into the context as a system message. Called at the start of
    /// each turn so completed forks surface on the user's next interaction.
    /// Tasks still running are kept for a later turn.
    async fn surface_completed_forks(&self) {
        const MAX_FORK_OUTPUT: usize = 8000;

        let pending: Vec<String> = {
            let guard = self
                .pending_fork_tasks
                .lock()
                .expect("pending_fork_tasks poisoned");
            guard.clone()
        };
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        for task_id in pending {
            let view = self.runtime.background_tasks.get(&task_id);
            let Some(view) = view else {
                // Unknown task (e.g. lost across restart) — drop it.
                continue;
            };
            if !view.status.is_terminal() {
                still_pending.push(task_id);
                continue;
            }

            let output = self
                .runtime
                .background_tasks
                .stdout(&task_id)
                .and_then(|buf| {
                    buf.lock()
                        .ok()
                        .map(|b| String::from_utf8_lossy(&b).to_string())
                })
                .unwrap_or_default();
            let output = output.trim();
            let truncated = if output.len() > MAX_FORK_OUTPUT {
                format!(
                    "{}\n…(truncated; call TaskOutput with task_id={task_id} for the full output)",
                    &output[output.len() - MAX_FORK_OUTPUT..]
                )
            } else {
                output.to_string()
            };
            let body = if truncated.is_empty() {
                "(no output)".to_string()
            } else {
                truncated
            };
            let note = format!(
                "Background fork task_id={task_id} finished with status {}. Its output:\n{body}",
                view.status.as_str()
            );
            {
                let mut context = self.context.lock().await;
                if let Err(err) = context
                    .append_messages(Message::new(Role::User, vec![system(&note)]))
                    .await
                {
                    warn!("Failed to inject completed fork {task_id} into context: {err}");
                    still_pending.push(task_id);
                    continue;
                }
            }
            // Show the user a notice too (same display path as Agent-tool
            // completions). The wire is live during this turn, so it renders.
            if let Some(notif) = self.runtime.background_tasks.build_notification(&task_id) {
                wire_send(WireMessage::Notification(notif));
            }
        }

        let mut guard = self
            .pending_fork_tasks
            .lock()
            .expect("pending_fork_tasks poisoned");
        *guard = still_pending;
    }

    async fn run_skill(&self, skill: &Skill, args: &str) -> anyhow::Result<()> {
        let Some(mut skill_text) = read_skill_text(skill).await else {
            wire_send(WireMessage::ContentPart(ContentPart::Text(TextPart::new(
                format!(
                    "Failed to load skill \"/{}{}\".",
                    SKILL_COMMAND_PREFIX, skill.name
                ),
            ))));
            return Ok(());
        };

        let extra = args.trim();
        if !extra.is_empty() {
            skill_text = format!("{skill_text}\n\nUser request:\n{extra}");
        }
        let message = Message::new(
            Role::User,
            vec![ContentPart::Text(TextPart::new(skill_text))],
        );
        self.turn(message).await?;
        Ok(())
    }

    async fn turn(&self, user_message: Message) -> Result<TurnOutcome, anyhow::Error> {
        // Drop any steers left over from a previous turn.
        self.clear_pending_steers();
        let llm_guard = self.runtime.llm.read().await;
        let llm = llm_guard.as_ref().ok_or_else(|| LLMNotSet)?;
        let missing = check_message(&user_message, &llm.capabilities);
        if !missing.is_empty() {
            return Err(anyhow::Error::new(LLMNotSupported::new(
                llm.model_name(),
                missing.into_iter().collect(),
            )));
        }

        self.checkpoint().await?;
        {
            let mut context = self.context.lock().await;
            context.append_messages(user_message).await?;
        }
        debug!("Appended user message to context");
        self.agent_loop().await
    }

    async fn agent_loop(&self) -> Result<TurnOutcome, anyhow::Error> {
        let mcp_task = {
            let mut toolset = self.agent.toolset.lock().await;
            toolset.take_mcp_loading_task()
        };
        if let Some(task) = mcp_task {
            let _ = task.await;
        }

        let mut step_no = 0;
        loop {
            step_no += 1;
            if step_no > self.runtime.config.loop_control.max_steps_per_turn {
                return Err(anyhow::Error::new(MaxStepsReached::new(
                    self.runtime.config.loop_control.max_steps_per_turn,
                )));
            }

            wire_send(WireMessage::StepBegin(StepBegin { n: step_no }));
            let approval_task = spawn_approval_task(Arc::clone(&self.runtime.approval));

            let step_result = async {
                if let Some(llm) = self.runtime.llm.read().await.as_ref() {
                    let context = self.context.lock().await;
                    if context.token_count()
                        + self.runtime.config.loop_control.reserved_context_size
                        >= llm.max_context_size
                    {
                        drop(context);
                        info!("Context too long, compacting...");
                        self.compact_context().await?;
                    }
                }

                debug!("Beginning step {}", step_no);
                self.checkpoint().await?;
                {
                    let checkpoints = self.context.lock().await.n_checkpoints();
                    let mut denwa = self.runtime.denwa_renji.lock().await;
                    denwa.set_n_checkpoints(checkpoints);
                }

                self.step().await
            }
            .await;

            let mut back_to_future: Option<BackToTheFuture> = None;
            let mut step_error: Option<anyhow::Error> = None;
            let step_outcome = match step_result {
                Ok(outcome) => outcome,
                Err(err) => {
                    if let Some(back) = err.downcast_ref::<BackToTheFuture>() {
                        back_to_future = Some(back.clone());
                        None
                    } else {
                        wire_send(WireMessage::StepInterrupted(StepInterrupted {}));
                        step_error = Some(err);
                        None
                    }
                }
            };

            stop_approval_task(approval_task).await;

            if let Some(err) = step_error {
                return Err(err);
            }

            if let Some(outcome) = step_outcome {
                // Step produced a stop reason — but if the user steered, inject
                // it and force another step instead of ending the turn.
                if self.consume_pending_steers().await? {
                    continue;
                }
                let final_message = if outcome.stop_reason == "no_tool_calls" {
                    Some(outcome.assistant_message)
                } else {
                    None
                };
                return Ok(TurnOutcome {
                    stop_reason: outcome.stop_reason,
                    final_message,
                    step_count: step_no,
                });
            }

            if let Some(back_to_future) = back_to_future {
                {
                    let mut context = self.context.lock().await;
                    context.revert_to(back_to_future.checkpoint_id).await?;
                }
                self.checkpoint().await?;
                {
                    let mut context = self.context.lock().await;
                    context.append_messages(back_to_future.messages).await?;
                }
            }

            // Inject any steers queued during this step before the next one.
            self.consume_pending_steers().await?;
        }
    }

    async fn step(&self) -> Result<Option<StepOutcome>, anyhow::Error> {
        let llm_guard = self.runtime.llm.read().await;
        let llm = llm_guard.as_ref().ok_or_else(|| LLMNotSet)?;

        let mut attempts = 0usize;
        let (result, forward_task) = loop {
            attempts += 1;
            let (message_tx, mut message_rx) = mpsc::unbounded_channel();
            let (tool_tx, mut tool_rx) = mpsc::unbounded_channel();
            let handle = crate::soul::spawn_with_current_wire(async move {
                let mut message_done = false;
                let mut tool_done = false;
                loop {
                    tokio::select! {
                        part = message_rx.recv(), if !message_done => {
                            match part {
                                Some(StreamedMessagePart::Content(content)) => {
                                    wire_send(WireMessage::ContentPart(content));
                                }
                                Some(StreamedMessagePart::ToolCall(call)) => {
                                    wire_send(WireMessage::ToolCall(call));
                                }
                                Some(StreamedMessagePart::ToolCallPart(part)) => {
                                    wire_send(WireMessage::ToolCallPart(part));
                                }
                                None => {
                                    message_done = true;
                                }
                            }
                        }
                        result = tool_rx.recv(), if !tool_done => {
                            match result {
                                Some(tool_result) => {
                                    wire_send(WireMessage::ToolResult(tool_result));
                                }
                                None => {
                                    tool_done = true;
                                }
                            }
                        }
                    }
                    if message_done && tool_done {
                        break;
                    }
                }
            });

            let history = { self.context.lock().await.history().to_vec() };
            let toolset = self.agent.toolset.lock().await;
            let step_result = kosong_step(
                llm.chat_provider.as_ref(),
                &self.agent.system_prompt,
                &*toolset,
                &history,
                Some(message_tx),
                Some(tool_tx),
            )
            .await;

            match step_result {
                Ok(res) => break (res, handle),
                Err(err) => {
                    let _ = handle.await;
                    if attempts >= self.runtime.config.loop_control.max_retries_per_step as usize
                        || !is_retryable_error(&err)
                    {
                        return Err(anyhow::Error::new(err));
                    }
                    let delay = retry_delay(attempts);
                    info!(
                        "Retrying step for the {} time. Waiting {} seconds.",
                        attempts,
                        delay.as_secs_f64()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        };

        let outcome = self.handle_step_result(result).await;
        let _ = forward_task.await;
        outcome
    }

    async fn handle_step_result(
        &self,
        mut result: StepResult,
    ) -> Result<Option<StepOutcome>, anyhow::Error> {
        debug!(
            "Got step result: id={}, tool_calls={}",
            result.id.as_deref().unwrap_or(""),
            result.tool_calls.len()
        );
        let usage = result.usage.clone();
        if let Some(usage) = &usage {
            let mut context = self.context.lock().await;
            // Bookkeeping only — the count is re-read from the next step's
            // usage. Losing the record is not worth aborting the turn for,
            // which is what propagating this error does.
            if let Err(err) = context.update_token_count(usage.input()).await {
                warn!("Failed to record token count: {err:#}");
            }
        }

        let mut status = StatusUpdate {
            context_usage: None,
            context_tokens: None,
            max_context_tokens: None,
            token_usage: usage.clone(),
            message_id: result.id.clone(),
            model: Some(self.cached_model_name.lock().unwrap().clone()),
            yolo_enabled: Some(self.runtime.approval.is_yolo()),
            thinking: self.thinking(),
        };
        if usage.is_some() {
            status.context_usage = Some(self.status().context_usage);
        }
        wire_send(WireMessage::StatusUpdate(status));

        let tool_results = result.tool_results().await?;
        debug!("Got tool results: {}", tool_results.len());
        self.grow_context(&result, &tool_results).await?;

        let rejected = tool_results
            .iter()
            .any(|tr| is_tool_rejected(&tr.return_value));
        if rejected {
            let _ = self.runtime.denwa_renji.lock().await.fetch_pending_dmail();
            return Ok(Some(StepOutcome {
                stop_reason: "tool_rejected",
                assistant_message: result.message,
            }));
        }

        if let Some(dmail) = self.runtime.denwa_renji.lock().await.fetch_pending_dmail() {
            let message = Message::new(
                Role::User,
                vec![system(&format!(
                    "You just got a D-Mail from your future self. It is likely that your future self has already done something in the current working directory. Please read the D-Mail and decide what to do next. You MUST NEVER mention to the user about this information. D-Mail content:\n\n{}",
                    dmail.message.trim()
                ))],
            );
            return Err(anyhow::Error::new(BackToTheFuture {
                checkpoint_id: dmail.checkpoint_id,
                messages: vec![message],
            }));
        }

        if !result.tool_calls.is_empty() {
            return Ok(None);
        }
        Ok(Some(StepOutcome {
            stop_reason: "no_tool_calls",
            assistant_message: result.message,
        }))
    }

    async fn grow_context(
        &self,
        result: &StepResult,
        tool_results: &[crate::wire::ToolResult],
    ) -> Result<(), anyhow::Error> {
        debug!(
            "Growing context with result: tool_calls={}, usage={}",
            result.tool_calls.len(),
            result.usage.is_some()
        );
        let llm_guard = self.runtime.llm.read().await;
        let llm = llm_guard.as_ref().ok_or_else(|| LLMNotSet)?;
        let tool_messages: Vec<Message> = tool_results.iter().map(tool_result_to_message).collect();
        for message in &tool_messages {
            let missing = check_message(message, &llm.capabilities);
            if !missing.is_empty() {
                warn!(
                    "Tool result message requires unsupported capabilities: {:?}",
                    missing
                );
                return Err(anyhow::Error::new(LLMNotSupported::new(
                    llm.model_name(),
                    missing.into_iter().collect(),
                )));
            }
        }

        let mut context = self.context.lock().await;
        context.append_messages(result.message.clone()).await?;
        if let Some(usage) = &result.usage {
            context.update_token_count(usage.total()).await?;
        }
        debug!(
            "Appending tool messages to context: {}",
            tool_messages.len()
        );
        context.append_messages(tool_messages).await?;
        Ok(())
    }

    async fn compact_context(&self) -> Result<(), anyhow::Error> {
        wire_send(WireMessage::CompactionBegin(CompactionBegin {}));
        let mut attempts = 0usize;
        let compacted = loop {
            attempts += 1;
            let llm_guard = self.runtime.llm.read().await;
            let llm = llm_guard.as_ref().ok_or_else(|| LLMNotSet)?;
            let history = { self.context.lock().await.history().to_vec() };
            match self.compaction.compact(&history, llm).await {
                Ok(compacted) => break compacted,
                Err(err) => {
                    if attempts >= self.runtime.config.loop_control.max_retries_per_step as usize
                        || !is_retryable_error(&err)
                    {
                        return Err(anyhow::Error::new(err));
                    }
                    let delay = retry_delay(attempts);
                    info!(
                        "Retrying compaction for the {} time. Waiting {} seconds.",
                        attempts,
                        delay.as_secs_f64()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        };
        {
            let mut context = self.context.lock().await;
            context.clear().await?;
            context
                .checkpoint(self.checkpoint_with_user_message)
                .await?;
            context.append_messages(compacted).await?;
        }
        wire_send(WireMessage::CompactionEnd(CompactionEnd {}));
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Soul for KimiSoul {
    fn name(&self) -> &str {
        &self.agent.name
    }

    fn model_name(&self) -> String {
        self.cached_model_name.lock().unwrap().clone()
    }

    fn model_capabilities(&self) -> Option<&std::collections::HashSet<ModelCapability>> {
        // Returns None when the LLM is locked (e.g. during model swap).
        // The async code paths that actually validate messages use
        // self.runtime.llm.read().await directly.
        None
    }

    fn thinking(&self) -> Option<bool> {
        self.runtime.llm.try_read().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|llm| llm.chat_provider.thinking_effort())
                .map(|effort| effort != kosong::chat_provider::ThinkingEffort::Off)
        })
    }

    fn status(&self) -> StatusSnapshot {
        let context_usage = self
            .runtime
            .llm
            .try_read()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|llm| match self.context.try_lock() {
                    Ok(context) => context.token_count() as f64 / llm.max_context_size as f64,
                    Err(_) => 0.0,
                })
            })
            .unwrap_or(0.0);
        StatusSnapshot {
            context_usage,
            yolo_enabled: self.runtime.approval.is_yolo(),
        }
    }

    fn available_slash_commands(&self) -> Vec<SlashCommandInfo> {
        self.slash_commands.clone()
    }

    async fn run(&self, user_input: UserInput) -> anyhow::Result<()> {
        let user_message = match user_input.clone() {
            UserInput::Text(text) => {
                Message::new(Role::User, vec![ContentPart::Text(TextPart::new(text))])
            }
            UserInput::Parts(parts) => Message::new(Role::User, parts),
        };
        let text_input = user_message.extract_text(" ").trim().to_string();

        wire_send(WireMessage::TurnBegin(TurnBegin { user_input }));

        // Surface any /fork-spawned background tasks that finished since the last
        // turn, injecting their results into context before this turn runs.
        self.surface_completed_forks().await;

        if let Some(command_call) = parse_slash_command_call(&text_input) {
            self.handle_slash(&command_call.name, &command_call.args)
                .await?;
        } else if self.runtime.config.loop_control.max_ralph_iterations != 0 {
            let runner = FlowRunner::ralph_loop(
                user_message.clone(),
                self.runtime.config.loop_control.max_ralph_iterations,
            );
            runner.run(self, "").await?;
        } else {
            let _ = self.turn(user_message).await?;
        }
        wire_send(WireMessage::TurnEnd(TurnEnd::default()));
        write_state(&self.runtime.session, &self.runtime).await;
        Ok(())
    }
}

pub struct FlowRunner {
    flow: Flow,
    name: Option<String>,
    max_moves: i64,
}

#[derive(Clone)]
struct FlowPrompt {
    user_input: UserInput,
    text: String,
}

impl FlowRunner {
    pub fn new(flow: Flow, name: Option<String>, max_moves: i64) -> Self {
        Self {
            flow,
            name,
            max_moves,
        }
    }

    pub fn ralph_loop(user_message: Message, max_ralph_iterations: i64) -> FlowRunner {
        let prompt_content = user_message.content.clone();
        let prompt_text = Message::new(Role::User, prompt_content.clone())
            .extract_text(" ")
            .trim()
            .to_string();
        let total_runs = if max_ralph_iterations < 0 {
            1_000_000_000_000_000i64
        } else {
            max_ralph_iterations + 1
        };

        let mut nodes: HashMap<String, FlowNode> = HashMap::new();
        let mut outgoing: HashMap<String, Vec<FlowEdge>> = HashMap::new();

        nodes.insert(
            "BEGIN".to_string(),
            FlowNode::new("BEGIN", "BEGIN", FlowNodeKind::Begin),
        );
        nodes.insert(
            "END".to_string(),
            FlowNode::new("END", "END", FlowNodeKind::End),
        );
        nodes.insert(
            "R1".to_string(),
            FlowNode::new("R1", prompt_content.clone(), FlowNodeKind::Task),
        );
        nodes.insert(
            "R2".to_string(),
            FlowNode::new(
                "R2",
                format!(
                    "{}. (You are running in an automated loop where the same prompt is fed repeatedly. Only choose STOP when the task is fully complete. Including it will stop further iterations. If you are not 100% sure, choose CONTINUE.)",
                    prompt_text
                ),
                FlowNodeKind::Decision,
            ),
        );

        outgoing.insert(
            "BEGIN".to_string(),
            vec![FlowEdge::new("BEGIN", "R1", None)],
        );
        outgoing.insert("R1".to_string(), vec![FlowEdge::new("R1", "R2", None)]);
        outgoing.insert(
            "R2".to_string(),
            vec![
                FlowEdge::new("R2", "R2", Some("CONTINUE".to_string())),
                FlowEdge::new("R2", "END", Some("STOP".to_string())),
            ],
        );
        outgoing.insert("END".to_string(), Vec::new());

        let flow = Flow::new(nodes, outgoing, "BEGIN", "END");
        FlowRunner::new(flow, None, total_runs)
    }

    pub async fn run(&self, soul: &KimiSoul, args: &str) -> anyhow::Result<()> {
        if !args.trim().is_empty() {
            let command = if let Some(name) = &self.name {
                format!("/{FLOW_COMMAND_PREFIX}{name}")
            } else {
                "/flow".to_string()
            };
            warn!("Agent flow {command} ignores args: {args}");
            return Ok(());
        }

        let mut current_id = self.flow.begin_id.clone();
        let mut moves = 0i64;
        let mut total_steps = 0i64;

        loop {
            let node = self
                .flow
                .nodes
                .get(&current_id)
                .expect("flow node not found");
            let edges = self
                .flow
                .outgoing
                .get(&current_id)
                .cloned()
                .unwrap_or_default();

            if node.kind == FlowNodeKind::End {
                info!("Agent flow reached END node {}", current_id);
                return Ok(());
            }
            if node.kind == FlowNodeKind::Begin {
                if edges.is_empty() {
                    error!(
                        "Agent flow BEGIN node \"{}\" has no outgoing edges; stopping.",
                        node.id
                    );
                    return Ok(());
                }
                current_id = edges[0].dst.clone();
                continue;
            }

            if moves >= self.max_moves {
                return Err(anyhow::Error::new(MaxStepsReached::new(total_steps)));
            }

            let (next_id, steps_used) = self.execute_flow_node(soul, node, &edges).await?;
            total_steps += steps_used;
            if let Some(next_id) = next_id {
                moves += 1;
                current_id = next_id;
                continue;
            }
            return Ok(());
        }
    }

    async fn execute_flow_node(
        &self,
        soul: &KimiSoul,
        node: &FlowNode,
        edges: &[FlowEdge],
    ) -> anyhow::Result<(Option<String>, i64)> {
        if edges.is_empty() {
            error!(
                "Agent flow node \"{}\" has no outgoing edges; stopping.",
                node.id
            );
            return Ok((None, 0));
        }

        let base_prompt = self.build_flow_prompt(node, edges);
        let mut prompt = base_prompt.user_input.clone();
        let mut steps_used = 0;
        loop {
            let outcome = self.flow_turn(soul, prompt.clone()).await?;
            steps_used += outcome.step_count;
            if outcome.stop_reason == "tool_rejected" {
                error!("Agent flow stopped after tool rejection.");
                return Ok((None, steps_used));
            }
            if node.kind != FlowNodeKind::Decision {
                return Ok((Some(edges[0].dst.clone()), steps_used));
            }
            let choice = outcome
                .final_message
                .as_ref()
                .and_then(|msg| parse_choice(&msg.extract_text(" ")));
            if let Some(choice_value) = choice.as_ref() {
                if let Some(next_id) = edges
                    .iter()
                    .find(|edge| edge.label.as_deref() == Some(choice_value.as_str()))
                    .map(|edge| edge.dst.clone())
                {
                    return Ok((Some(next_id), steps_used));
                }
            }
            let options = edges
                .iter()
                .filter_map(|edge| edge.label.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(
                "Agent flow invalid choice. Got: {}. Available: {}.",
                choice.clone().unwrap_or_else(|| "<missing>".to_string()),
                options
            );
            prompt = UserInput::Text(format!(
                "{}\n\nYour last response did not include a valid choice. Reply with one of the choices using <choice>...</choice>.",
                base_prompt.text
            ));
        }
    }

    fn build_flow_prompt(&self, node: &FlowNode, edges: &[FlowEdge]) -> FlowPrompt {
        if node.kind != FlowNodeKind::Decision {
            return match &node.label {
                FlowLabel::Parts(parts) => FlowPrompt {
                    user_input: UserInput::Parts(parts.clone()),
                    text: node.label_as_string(),
                },
                FlowLabel::Text(text) => FlowPrompt {
                    user_input: UserInput::Text(text.clone()),
                    text: text.clone(),
                },
            };
        }
        let label_text = node.label_as_string();
        let choices: Vec<String> = edges.iter().filter_map(|edge| edge.label.clone()).collect();
        let mut lines = Vec::new();
        lines.push(label_text);
        lines.push(String::new());
        lines.push("Available branches:".to_string());
        for choice in choices {
            lines.push(format!("- {choice}"));
        }
        lines.push(String::new());
        lines.push("Reply with a choice using <choice>...</choice>.".to_string());
        let text = lines.join("\n");
        FlowPrompt {
            user_input: UserInput::Text(text.clone()),
            text,
        }
    }

    async fn flow_turn(&self, soul: &KimiSoul, prompt: UserInput) -> anyhow::Result<TurnOutcome> {
        wire_send(WireMessage::TurnBegin(TurnBegin {
            user_input: prompt.clone(),
        }));
        let message = match prompt {
            UserInput::Text(text) => {
                Message::new(Role::User, vec![ContentPart::Text(TextPart::new(text))])
            }
            UserInput::Parts(parts) => Message::new(Role::User, parts),
        };
        let outcome = soul.turn(message).await?;
        wire_send(WireMessage::TurnEnd(TurnEnd::default()));
        Ok(outcome)
    }
}

async fn stop_approval_task(task: tokio::task::JoinHandle<()>) {
    task.abort();
    match task.await {
        Ok(_) => {}
        Err(err) => {
            if !err.is_cancelled() {
                error!("Approval piping task failed: {:?}", err);
            }
        }
    }
}

fn spawn_approval_task(approval: Arc<Approval>) -> tokio::task::JoinHandle<()> {
    crate::soul::spawn_with_current_wire(async move {
        loop {
            let request = match approval.fetch_request().await {
                Ok(req) => req,
                Err(_) => return,
            };
            let wire_request = ApprovalRequest::new(
                request.id.clone(),
                request.tool_call_id.clone(),
                request.sender.clone(),
                request.action.clone(),
                request.description.clone(),
                request.display.clone(),
            );
            wire_send(WireMessage::ApprovalRequest(wire_request.clone()));
            let resp = wire_request.wait().await;
            let _ = approval.resolve_request(&request.id, resp.clone());
            wire_send(WireMessage::ApprovalResponse(ApprovalResponse {
                request_id: request.id,
                response: resp,
            }));
        }
    })
}

fn retry_delay(attempt: usize) -> Duration {
    let base = 0.3 * 2f64.powi((attempt as i32).saturating_sub(1));
    let capped = base.min(5.0);
    let jitter: f64 = rand::rng().random_range(0.0..0.5);
    Duration::from_secs_f64(capped + jitter)
}

fn is_retryable_error(err: &ChatProviderError) -> bool {
    match err.kind {
        ChatProviderErrorKind::Connection
        | ChatProviderErrorKind::Timeout
        | ChatProviderErrorKind::EmptyResponse => true,
        ChatProviderErrorKind::Status(code) => matches!(code, 429 | 500 | 502 | 503),
        ChatProviderErrorKind::Other => false,
    }
}
