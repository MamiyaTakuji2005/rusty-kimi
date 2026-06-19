Spawn a subagent to delegate a specific task you might not have enough context headroom  at the moment or you want to do work in paralell. Think of this of a high investment / high return operation that only makes sense on big tasks
**DON'TS**
- DO NOT directly forward the user prompt to the subagent. 
- Don't spawn a subagent just to wait, or worse use task output the whole time just to watch it and waste time/ get locked in the token stream.
**DO'S**
- Subagent will be spawned with a fresh context window and no prior history or knowledge about your current task or objective.
- Prompt it with explicit goals on when the job is finished and as much information like direcotry names
- Context isolation is one of the major benefits but if you're a model with 1 million context you're less likely to benefit.
- Parallel tasking is another key benefit of this tool. When your task involves multiple seperate subtasks that are independent of each other, multiple times in a single response to let subagents work in parallel for you.
**Available Subagents:**

${SUBAGENTS_MD}
