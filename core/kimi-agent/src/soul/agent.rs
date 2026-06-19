use std::path::Path;
use std::sync::Arc;

use chrono::Local;
use kaos::{CachedKaos, KaosPath, set_current_kaos};
use regex::Regex;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::agentspec::load_agent_spec;
use crate::config::Config;
use crate::exception::{AgentSpecError, SystemPromptTemplateError};
use crate::llm::LLM;
use crate::session::Session;
use crate::skill::{Skill, discover_skills_from_roots, index_skills, resolve_skills_roots};
use crate::soul::approval::Approval;
use crate::soul::denwarenji::DenwaRenji;
use crate::soul::toolset::KimiToolset;
use crate::tasks::BackgroundTaskManager;
use crate::tools::todo::TodoItem;
use crate::utils::{Environment, list_directory};

use std::collections::HashMap;

#[derive(Clone, Debug)]
#[allow(non_snake_case)]
pub struct BuiltinSystemPromptArgs {
    pub KIMI_NOW: String,
    pub KIMI_WORK_DIR: KaosPath,
    pub KIMI_WORK_DIR_LS: String,
    pub KIMI_AGENTS_MD: String,
    pub KIMI_SKILLS: String,
}

impl BuiltinSystemPromptArgs {
    fn as_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("KIMI_NOW".to_string(), self.KIMI_NOW.clone());
        map.insert(
            "KIMI_WORK_DIR".to_string(),
            self.KIMI_WORK_DIR.to_string_lossy(),
        );
        map.insert(
            "KIMI_WORK_DIR_LS".to_string(),
            self.KIMI_WORK_DIR_LS.clone(),
        );
        map.insert("KIMI_AGENTS_MD".to_string(), self.KIMI_AGENTS_MD.clone());
        map.insert("KIMI_SKILLS".to_string(), self.KIMI_SKILLS.clone());
        map
    }
}

pub async fn load_agents_md(work_dir: &KaosPath) -> Option<String> {
    let candidates = [
        work_dir.clone() / "AGENTS.md",
        work_dir.clone() / "agents.md",
    ];
    for path in candidates {
        if path.is_file(true).await {
            if let Ok(text) = path.read_text().await {
                info!("Loaded agents.md: {}", path.to_string_lossy());
                return Some(text.trim().to_string());
            }
        }
    }
    info!("No AGENTS.md found in {}", work_dir.to_string_lossy());
    None
}

#[derive(Clone)]
pub struct Runtime {
    pub config: Config,
    pub llm: Arc<RwLock<Option<Arc<LLM>>>>,
    pub session: Session,
    pub builtin_args: BuiltinSystemPromptArgs,
    pub denwa_renji: Arc<tokio::sync::Mutex<DenwaRenji>>,
    pub approval: Arc<Approval>,
    pub environment: Environment,
    pub skills: HashMap<String, Skill>,
    pub background_tasks: BackgroundTaskManager,
    pub todos: Arc<std::sync::Mutex<Vec<TodoItem>>>,
    pub cached_kaos: Arc<CachedKaos>,
}

impl Runtime {
    pub async fn create(
        config: Config,
        llm: Option<Arc<LLM>>,
        session: Session,
        yolo: bool,
        skills_dir: Option<KaosPath>,
    ) -> Runtime {
        let work_dir = session.work_dir.clone();

        // Build the cached kaos index in parallel with the other startup I/O.
        // CachedKaos::new does a blocking ignore-aware full scan of work_dir so
        // the first Glob tool call hits memory instead of walking the disk.
        let (cached_kaos_built, ls_output, agents_md, environment) = tokio::join!(
            CachedKaos::new(work_dir.as_path().to_path_buf()),
            list_directory(&work_dir),
            load_agents_md(&work_dir),
            Environment::detect()
        );
        let cached_kaos = Arc::new(cached_kaos_built);
        // Install as global kaos. Runtime::create runs once at startup outside
        // any task-local scope, so this sets the process-wide backend.
        // CurrentKaosToken has no Drop impl — dropping it does not reset the global.
        let _ = set_current_kaos(Arc::clone(&cached_kaos) as Arc<dyn kaos::Kaos>);

        let skills_roots = resolve_skills_roots(&work_dir, skills_dir).await;
        let skills = discover_skills_from_roots(&skills_roots).await;
        info!("Discovered {} skill(s)", skills.len());
        let skills_by_name = index_skills(&skills);
        let skills_formatted = if skills.is_empty() {
            "No skills found.".to_string()
        } else {
            skills
                .iter()
                .map(|skill| {
                    format!(
                        "- {}\n  - Path: {}\n  - Description: {}",
                        skill.name,
                        skill.skill_md_file().to_string_lossy(),
                        skill.description
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let tasks_dir = session
            .context_file
            .parent()
            .map(|d| d.join("tasks"))
            .unwrap_or_else(|| std::path::PathBuf::from("tasks"));
        let session_id = session.id.clone();

        Runtime {
            config,
            llm: Arc::new(RwLock::new(llm)),
            session,
            builtin_args: BuiltinSystemPromptArgs {
                KIMI_NOW: Local::now().to_rfc3339(),
                KIMI_WORK_DIR: work_dir,
                KIMI_WORK_DIR_LS: ls_output,
                KIMI_AGENTS_MD: agents_md.unwrap_or_default(),
                KIMI_SKILLS: skills_formatted,
            },
            denwa_renji: Arc::new(tokio::sync::Mutex::new(DenwaRenji::new())),
            approval: Arc::new(Approval::new(yolo)),
            environment,
            skills: skills_by_name,
            background_tasks: BackgroundTaskManager::new(tasks_dir, session_id),
            todos: Arc::new(std::sync::Mutex::new(Vec::new())),
            cached_kaos,
        }
    }

}

#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub system_prompt: String,
    pub toolset: Arc<tokio::sync::Mutex<KimiToolset>>,
    pub runtime: Runtime,
}

pub fn load_agent<'a>(
    agent_file: &'a Path,
    runtime: Runtime,
    mcp_configs: &'a [serde_json::Value],
    extra_args: &'a HashMap<String, String>,
) -> futures::future::BoxFuture<'a, Result<Agent, anyhow::Error>> {
    Box::pin(async move {
        info!("Loading agent: {}", agent_file.display());
        let agent_spec = load_agent_spec(agent_file).await?;
        let system_prompt = load_system_prompt(
            &agent_spec.system_prompt_path,
            &agent_spec.system_prompt_args,
            &runtime.builtin_args,
            extra_args,
        )
        .await?;

        let toolset = Arc::new(tokio::sync::Mutex::new(KimiToolset::new()));
        {
            let mut guard = toolset.lock().await;
            let mut tools = agent_spec.tools.clone();
            if !agent_spec.exclude_tools.is_empty() {
                debug!("Excluding tools: {:?}", agent_spec.exclude_tools);
                tools.retain(|tool| !agent_spec.exclude_tools.contains(tool));
            }
            guard
                .load_tools(&tools, &runtime, Arc::clone(&toolset))
                .map_err(anyhow::Error::from)?;

            if !mcp_configs.is_empty() {
                guard
                    .load_mcp_tools(mcp_configs, &runtime, Arc::clone(&toolset))
                    .await?;
            }
        }

        Ok(Agent {
            name: agent_spec.name,
            system_prompt,
            toolset,
            runtime,
        })
    })
}

async fn load_system_prompt(
    path: &Path,
    args: &HashMap<String, String>,
    builtin_args: &BuiltinSystemPromptArgs,
    extra_args: &HashMap<String, String>,
) -> Result<String, anyhow::Error> {
    info!("Loading system prompt: {}", path.display());
    let system_prompt = tokio::fs::read_to_string(path).await.map_err(|err| {
        AgentSpecError::new(format!(
            "Failed to read system prompt {}: {err}",
            path.display()
        ))
    })?;

    // Merge order: builtins → spec args → extra args (extra wins).
    let mut values = builtin_args.as_map();
    for (key, value) in args {
        values.insert(key.clone(), value.clone());
    }
    for (key, value) in extra_args {
        values.insert(key.clone(), value.clone());
    }
    debug!(
        "Substituting system prompt with builtin args: {:?}, spec args: {:?}, extra args: {:?}",
        builtin_args.as_map(),
        args,
        extra_args
    );

    let rendered = substitute_template(system_prompt.trim(), &values).map_err(|missing| {
        SystemPromptTemplateError::new(format!(
            "Missing system prompt arg in {}: {}",
            path.display(),
            missing.join(", ")
        ))
    })?;
    Ok(rendered)
}

fn substitute_template(
    template: &str,
    values: &HashMap<String, String>,
) -> Result<String, Vec<String>> {
    let re = Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("valid system prompt placeholder regex");

    let mut missing: Vec<String> = Vec::new();
    let result = re
        .replace_all(template, |caps: &regex::Captures<'_>| {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            match values.get(key) {
                Some(value) => value.clone(),
                None => {
                    missing.push(key.to_string());
                    caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                }
            }
        })
        .to_string();

    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(missing);
    }

    Ok(result)
}
