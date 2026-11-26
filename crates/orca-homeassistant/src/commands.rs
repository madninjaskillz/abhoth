//! HA commands (SUPERVISOR.md 11.2 "Commands").
//!
//! Commands arrive on `abhoth/<server>/set/<obj>`. They are parsed and checked here, then
//! handed to a [`ControlPort`] as the MQTT **service principal** (D-SV12). The control port is
//! the switch orchestrator and card actions (rc-switch), which audit the call and apply the
//! same guards and calendar as any other caller. Nothing here can bypass them: this module
//! only ever *asks*.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::topics::{Topics, slug};

/// Largest command payload accepted (a layout id or a short JSON body).
pub const MAX_PAYLOAD: usize = 512;

/// The MQTT service principal: a D140 service user with exactly these scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePrincipal {
    /// Audit name, e.g. `mqtt:deepthought`.
    pub name: String,
    pub scopes: Vec<String>,
}

impl ServicePrincipal {
    pub fn mqtt(server: &str, scopes: &[String]) -> Self {
        Self {
            name: format!("mqtt:{}", slug(server)),
            scopes: scopes.to_vec(),
        }
    }

    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    Auto,
    Quiet,
    /// Full speed, expires after 1 h.
    Full,
}

impl FanMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "quiet" => Some(Self::Quiet),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// A parsed, validated HA command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Move the layout select (local only, no action).
    SelectLayout(String),
    /// Apply the selected (or named) layout through the switch orchestrator.
    ApplyLayout {
        layout: Option<String>,
    },
    CardOnline {
        card: String,
        online: bool,
    },
    /// `None` clears the cap.
    ClockCap {
        card: String,
        mhz: Option<u32>,
        ttl: Duration,
    },
    FanMode {
        mode: FanMode,
        ttl: Option<Duration>,
    },
}

impl Command {
    /// The scope the command needs (`None` = local-only).
    pub fn scope(&self) -> Option<&'static str> {
        match self {
            Self::SelectLayout(_) => None,
            Self::ApplyLayout { .. } => Some("switch"),
            Self::CardOnline { .. } => Some("card_state"),
            Self::ClockCap { .. } => Some("clock_cap"),
            Self::FanMode { .. } => Some("fan_profile"),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SelectLayout(_) => "layout_target",
            Self::ApplyLayout { .. } => "layout_apply",
            Self::CardOnline { .. } => "card_online",
            Self::ClockCap { .. } => "clock_cap",
            Self::FanMode { .. } => "fan_mode",
        }
    }
}

/// What the control port says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlOutcome {
    /// Accepted (possibly queued); `job` names the switch job when there is one.
    Accepted {
        message: String,
        job: Option<String>,
    },
    /// Already in that state.
    Already { message: String },
    /// Refused by a guard, the calendar, a lease, a limit or the principal's scopes.
    Refused { reason: String },
}

impl ControlOutcome {
    pub fn ok(&self) -> bool {
        !matches!(self, Self::Refused { .. })
    }
    pub fn message(&self) -> &str {
        match self {
            Self::Accepted { message, .. } | Self::Already { message } => message,
            Self::Refused { reason } => reason,
        }
    }
}

/// The controls HA may ask for. Implemented by the supervisor over the switch orchestrator
/// and card actions (rc-switch); every call is audited there under `who`, and refused by the
/// same guards, calendar and limits as any other caller.
#[async_trait]
pub trait ControlPort: Send + Sync {
    async fn switch_layout(&self, who: &ServicePrincipal, layout: &str) -> ControlOutcome;
    async fn set_card_online(
        &self,
        who: &ServicePrincipal,
        card: &str,
        online: bool,
    ) -> ControlOutcome;
    async fn set_clock_cap(
        &self,
        who: &ServicePrincipal,
        card: &str,
        mhz: Option<u32>,
        ttl: Duration,
    ) -> ControlOutcome;
    async fn set_fan_mode(
        &self,
        who: &ServicePrincipal,
        mode: FanMode,
        ttl: Option<Duration>,
    ) -> ControlOutcome;
}

/// What the parser needs to validate a command against the current state.
#[derive(Debug, Clone)]
pub struct CommandContext<'a> {
    pub layouts: &'a [String],
    pub cards: &'a [(String, bool)], // (id, cap offered)
    pub clock_cap_options: &'a [u32],
    pub clock_cap_ttl: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("not a command topic")]
    NotCommand,
    /// An empty payload: the clearing of a retained command (ours or anyone's). Ignored.
    #[error("empty payload")]
    Empty,
    #[error("retained command ignored (commands are never retained)")]
    Retained,
    #[error("payload too large")]
    TooLarge,
    #[error("unknown command {0:?}")]
    Unknown(String),
    #[error("{0}")]
    Invalid(String),
}

/// Parses `topic` + `payload` into a command. Retained messages on command topics are always
/// refused: a stale retained command replaying on every reconnect is how ha-bridge once
/// silently turned quiet mode off.
pub fn parse(
    t: &Topics,
    topic: &str,
    payload: &[u8],
    retained: bool,
    ctx: &CommandContext<'_>,
) -> Result<Command, ParseError> {
    let obj = topic
        .strip_prefix(&format!("{}/set/", t.base))
        .ok_or(ParseError::NotCommand)?;
    if payload.is_empty() {
        return Err(ParseError::Empty);
    }
    if retained {
        return Err(ParseError::Retained);
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(ParseError::TooLarge);
    }
    let p = std::str::from_utf8(payload)
        .map_err(|_| ParseError::Invalid("payload is not UTF-8".into()))?
        .trim();
    match obj {
        "layout_target" => {
            if ctx.layouts.iter().any(|l| l == p) {
                Ok(Command::SelectLayout(p.to_string()))
            } else {
                Err(ParseError::Invalid(format!("unknown layout {p:?}")))
            }
        }
        "layout_apply" => {
            // `apply` (the button) applies the selected layout; `{"layout": "x"}` names one.
            if p.starts_with('{') {
                let v: Value =
                    serde_json::from_str(p).map_err(|_| ParseError::Invalid("bad JSON".into()))?;
                if v.get("force").is_some() {
                    return Err(ParseError::Invalid(
                        "force is not available from Home Assistant".into(),
                    ));
                }
                let l = v
                    .get("layout")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ParseError::Invalid("missing layout".into()))?;
                if !ctx.layouts.iter().any(|x| x == l) {
                    return Err(ParseError::Invalid(format!("unknown layout {l:?}")));
                }
                Ok(Command::ApplyLayout {
                    layout: Some(l.to_string()),
                })
            } else if p == "apply" || p == "PRESS" {
                Ok(Command::ApplyLayout { layout: None })
            } else {
                Err(ParseError::Invalid(format!("unexpected payload {p:?}")))
            }
        }
        "fan_mode" => {
            let mode = FanMode::parse(p)
                .ok_or_else(|| ParseError::Invalid(format!("unknown fan mode {p:?}")))?;
            let ttl = (mode == FanMode::Full).then(|| Duration::from_secs(3600));
            Ok(Command::FanMode { mode, ttl })
        }
        other => {
            let rest = other
                .strip_prefix("card_")
                .ok_or_else(|| ParseError::Unknown(other.into()))?;
            let (card_slug, what) = rest
                .split_once('_')
                .ok_or_else(|| ParseError::Unknown(other.into()))?;
            let (card, cap_offered) = ctx
                .cards
                .iter()
                .find(|(c, _)| slug(c) == card_slug)
                .map(|(c, p)| (c.clone(), *p))
                .ok_or_else(|| ParseError::Invalid(format!("unknown card {card_slug:?}")))?;
            match what {
                "online" => match p {
                    "ON" => Ok(Command::CardOnline { card, online: true }),
                    "OFF" => Ok(Command::CardOnline {
                        card,
                        online: false,
                    }),
                    _ => Err(ParseError::Invalid(format!("unexpected payload {p:?}"))),
                },
                "clock_cap" => {
                    if !cap_offered {
                        return Err(ParseError::Invalid(format!(
                            "the clock cap is not offered for card {card} (allow_cap = false)"
                        )));
                    }
                    let mhz = if p == "none" {
                        None
                    } else {
                        let m: u32 = p
                            .parse()
                            .map_err(|_| ParseError::Invalid(format!("bad clock cap {p:?}")))?;
                        if !ctx.clock_cap_options.contains(&m) {
                            return Err(ParseError::Invalid(format!(
                                "clock cap {m} is not an offered option"
                            )));
                        }
                        Some(m)
                    };
                    Ok(Command::ClockCap {
                        card,
                        mhz,
                        ttl: ctx.clock_cap_ttl,
                    })
                }
                _ => Err(ParseError::Unknown(other.into())),
            }
        }
    }
}

/// Runs a command against the control port as `who`. Local-only commands return `None`.
pub async fn dispatch(
    port: &dyn ControlPort,
    who: &ServicePrincipal,
    cmd: &Command,
    selected: Option<&str>,
) -> Option<ControlOutcome> {
    if let Some(scope) = cmd.scope()
        && !who.has(scope)
    {
        return Some(ControlOutcome::Refused {
            reason: format!("the MQTT principal lacks the {scope} scope"),
        });
    }
    Some(match cmd {
        Command::SelectLayout(_) => return None,
        Command::ApplyLayout { layout } => {
            let Some(l) = layout.as_deref().or(selected) else {
                return Some(ControlOutcome::Refused {
                    reason: "no layout selected".into(),
                });
            };
            port.switch_layout(who, l).await
        }
        Command::CardOnline { card, online } => port.set_card_online(who, card, *online).await,
        Command::ClockCap { card, mhz, ttl } => port.set_clock_cap(who, card, *mhz, *ttl).await,
        Command::FanMode { mode, ttl } => port.set_fan_mode(who, *mode, *ttl).await,
    })
}

/// The `abhoth/<server>/result` payload and the `last_command` state attribute.
pub fn result_json(cmd: &Command, outcome: &ControlOutcome, at: &str) -> Value {
    let mut v =
        json!({"command": cmd.name(), "ok": outcome.ok(), "message": outcome.message(), "at": at});
    match cmd {
        Command::ApplyLayout { layout: Some(l) } => v["layout"] = json!(l),
        Command::CardOnline { card, online } => {
            v["card"] = json!(card);
            v["online"] = json!(online);
        }
        Command::ClockCap { card, mhz, ttl } => {
            v["card"] = json!(card);
            v["mhz"] = json!(mhz);
            v["ttl_seconds"] = json!(ttl.as_secs());
        }
        Command::FanMode { mode, .. } => v["mode"] = json!(mode),
        _ => {}
    }
    if let ControlOutcome::Accepted { job: Some(j), .. } = outcome {
        v["job"] = json!(j);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(layouts: &'a [String], cards: &'a [(String, bool)]) -> CommandContext<'a> {
        CommandContext {
            layouts,
            cards,
            clock_cap_options: &[1200, 1300, 1400, 1500],
            clock_cap_ttl: Duration::from_secs(3600),
        }
    }

    #[test]
    fn parses_and_refuses() {
        let t = Topics::new("abhoth", "homeassistant", "deepthought");
        let layouts = vec!["flash-x2".to_string(), "dense-x2".to_string()];
        let cards = vec![("A".to_string(), false), ("B".to_string(), true)];
        let c = ctx(&layouts, &cards);
        let p = |topic: &str, pl: &str| {
            parse(
                &t,
                &format!("abhoth/deepthought/set/{topic}"),
                pl.as_bytes(),
                false,
                &c,
            )
        };
        assert_eq!(
            p("layout_target", "dense-x2"),
            Ok(Command::SelectLayout("dense-x2".into()))
        );
        assert!(matches!(
            p("layout_target", "nope"),
            Err(ParseError::Invalid(_))
        ));
        assert_eq!(
            p("layout_apply", "apply"),
            Ok(Command::ApplyLayout { layout: None })
        );
        assert_eq!(
            p("layout_apply", r#"{"layout":"flash-x2"}"#),
            Ok(Command::ApplyLayout {
                layout: Some("flash-x2".into())
            })
        );
        assert!(matches!(
            p("layout_apply", r#"{"layout":"flash-x2","force":true}"#),
            Err(ParseError::Invalid(_))
        ));
        assert_eq!(
            p("card_b_online", "OFF"),
            Ok(Command::CardOnline {
                card: "B".into(),
                online: false
            })
        );
        assert_eq!(
            p("card_b_clock_cap", "1300"),
            Ok(Command::ClockCap {
                card: "B".into(),
                mhz: Some(1300),
                ttl: Duration::from_secs(3600)
            })
        );
        assert_eq!(
            p("card_b_clock_cap", "none"),
            Ok(Command::ClockCap {
                card: "B".into(),
                mhz: None,
                ttl: Duration::from_secs(3600)
            })
        );
        assert!(
            matches!(p("card_a_clock_cap", "1300"), Err(ParseError::Invalid(m)) if m.contains("not offered"))
        );
        assert!(matches!(
            p("card_b_clock_cap", "900"),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            p("card_z_online", "ON"),
            Err(ParseError::Invalid(_))
        ));
        assert_eq!(
            p("fan_mode", "full"),
            Ok(Command::FanMode {
                mode: FanMode::Full,
                ttl: Some(Duration::from_secs(3600))
            })
        );
        assert!(matches!(p("reboot", "now"), Err(ParseError::Unknown(_))));
        assert_eq!(
            parse(&t, "abhoth/deepthought/set/fan_mode", b"auto", true, &c),
            Err(ParseError::Retained)
        );
        assert_eq!(
            parse(&t, "abhoth/deepthought/set/fan_mode", b"", false, &c),
            Err(ParseError::Empty)
        );
        assert_eq!(
            parse(&t, "abhoth/other/set/fan_mode", b"auto", false, &c),
            Err(ParseError::NotCommand)
        );
        assert_eq!(
            parse(
                &t,
                "abhoth/deepthought/set/fan_mode",
                &[b'a'; 600],
                false,
                &c
            ),
            Err(ParseError::TooLarge)
        );
    }
}
