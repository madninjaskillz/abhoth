//! The Pi tool profile (`toolProfile: "pi"`): `PiProfileWiring.Register`,
//! `AgentWiring.RegisterPiAgentTools` and `ToolExposure.FilterPiProfile`. Pi's tools replace
//! the OpenOrca tools of the same name ("bash", "grep"); every other OpenOrca tool stays
//! registered (callable) but is not advertised (`orca_core::agent::tool_exposure`).

use std::sync::Arc;

use orca_core::agent::ToolRegistry;
use orca_core::config::OrcaConfig;
use orca_tools_pi::{lead_mode, progress, subagent};

/// `AgentTypeRegistry.UsePiProfile`'s types (name, description), ordered by name.
pub const PI_AGENT_TYPES: [(&str, &str); 4] = [
    (
        "planner",
        "Turns a goal plus context into a concrete implementation plan; does not modify files",
    ),
    (
        "reviewer",
        "Reviews code or changes for bugs, risks and quality; does not modify files",
    ),
    (
        "scout",
        "Fast codebase recon; returns files with line ranges, key findings and where to start, for handoff",
    ),
    (
        "worker",
        "Builds and changes code with full capabilities (read, bash, edit, write); give it one self-contained piece of the work",
    ),
];

/// Registers the Pi profile's tools on `r` and limits what is advertised to Pi's set.
pub fn register(r: &mut ToolRegistry, config: &OrcaConfig) {
    let shell = match orca_tools_pi::shell::resolve(config.pi.shell_path.as_deref()) {
        Ok(s) => {
            tracing::info!("Pi profile: bash = {}", s.path);
            Some(s)
        }
        Err(e) => {
            // Fall back to OpenOrca's own shell tool rather than failing every call.
            tracing::warn!("Pi profile: no bash found, keeping OpenOrca's bash tool. {e}");
            eprintln!(
                "\u{26A0} Pi profile: no bash shell found \u{2014} falling back to OpenOrca's shell tool. Install Git for Windows or set pi.shellPath in config.json."
            );
            None
        }
    };
    if !config.lm_studio.native_tool_calling {
        tracing::warn!(
            "Pi profile assumes native tool calling, but lmStudio.nativeToolCalling is off"
        );
    }
    let has_shell = shell.is_some();
    let tools = orca_tools_pi::create_toolset(&orca_tools_pi::PiToolsetOptions {
        extra_tools: config.pi.extra_tools,
        jobs: config.pi.jobs,
        shell,
        bash_default_timeout_seconds: config.pi.bash_timeout_seconds.max(0),
        session_environment: session_environment(config),
        job_manager: None,
    });
    for tool in tools {
        if tool.name() == "bash" && !has_shell {
            continue;
        }
        r.register(as_tool(tool));
    }

    let agent = &config.agent;
    let auto_lead = !agent.peer_mode && agent.lead_mode.trim().eq_ignore_ascii_case("auto");
    if config.pi.agent_tools {
        // Gap: custom agent types are not listed (the Rust CLI has no agent registry yet).
        let agent_list = PI_AGENT_TYPES
            .iter()
            .map(|(n, d)| format!("{n} ({d})"))
            .collect::<Vec<_>>()
            .join(", ");
        // No orchestrator in the Rust CLI yet: the subagent tool stays unwired and answers
        // "Subagents are not available in this session."
        r.register(Arc::new(subagent::PiSubagentTool::new(
            subagent::PiSubagentSettings {
                agent_list,
                peer_mode: agent.peer_mode,
                offers_wait: agent.peer_mode && agent.peer_background,
                orchestrator_lead: orca_core::agent::policies::is_orchestrator_lead(config),
                auto_lead,
            },
        )));
        let progress_tool = progress::PiProgressTool::default();
        // The Lead's bar (AgentOrchestrator.ReportProgress outside a sub-agent).
        progress_tool.set_report(Some(Arc::new(|percent: i32, _note: Option<&str>| {
            format!("Progress recorded: {}%.", percent.clamp(0, 100))
        })));
        r.register(Arc::new(progress_tool));
        if auto_lead {
            // Unwired (no orchestrator): "lead_mode is not available in this session."
            r.register(Arc::new(lead_mode::PiLeadModeTool::default()));
        }
    }

    // What is advertised is `tool_exposure::is_advertised` (FilterPiProfile); its order is the
    // registration order, so MCP tools and `tools.extraTools` names move behind the Pi tools
    // (MCP servers started later register theirs after them anyway).
    for name in r.names() {
        if (name.to_ascii_lowercase().starts_with("mcp_")
            || config
                .tools
                .extra_tools
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&name)))
            && !orca_tools_pi::advertised_names(config.pi.extra_tools, config.pi.jobs)
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&name))
            && let Some(tool) = r.resolve(&name)
        {
            r.register(tool);
        }
    }
}

/// Kills the Pi background jobs this process started (session end).
pub fn kill_jobs() {
    orca_tools_pi::PiJobManager::shared().kill_all();
}

/// Pi's PI_* variables, as far as OpenOrca knows them at startup.
fn session_environment(config: &OrcaConfig) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let Some(model) = config.lm_studio.primary_model().filter(|m| !m.is_empty()) else {
        return env;
    };
    env.push(("PI_MODEL".to_string(), model.to_string()));
    if let Some((provider, _)) = config
        .providers
        .iter()
        .find(|(_, p)| p.models.iter().any(|m| m.eq_ignore_ascii_case(model)))
        && !provider.is_empty()
    {
        env.push(("PI_PROVIDER".to_string(), provider.clone()));
    }
    env
}

fn as_tool(tool: Arc<dyn orca_tools_pi::PiTool>) -> Arc<dyn orca_abstractions::Tool> {
    tool
}
