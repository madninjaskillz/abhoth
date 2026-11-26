//! Fan inputs from sensor expressions (orca-sensors): each fan's `temp` and `load` are
//! [`orca_sensors::Expr`]s evaluated against the sensor store. Missing stays missing (D-SV4):
//! the fan loop then pushes nothing (device curve) or fails safe (host curve).

use orca_sensors::expr::EvalState;
use orca_sensors::{Expr, ExprError, SensorSource, TimestampMs};

use crate::controller::{FanInputs, FansPolicy, Sample};

struct FanExprs {
    temp: (Expr, EvalState),
    load: Option<(Expr, EvalState)>,
}

/// The compiled expressions of every fan, with their evaluation state (`ema`s).
pub struct ExprInputs {
    fans: Vec<FanExprs>,
}

impl ExprInputs {
    pub fn new(policy: &FansPolicy) -> Result<ExprInputs, (String, ExprError)> {
        let mut fans = Vec::new();
        for f in &policy.fans {
            let temp = Expr::parse(&f.temp).map_err(|e| (format!("fans.{}.temp", f.id), e))?;
            let load = match &f.load {
                Some(l) => Some(Expr::parse(l).map_err(|e| (format!("fans.{}.load", f.id), e))?),
                None => None,
            };
            fans.push(FanExprs {
                temp: {
                    let s = temp.new_state();
                    (temp, s)
                },
                load: load.map(|l| {
                    let s = l.new_state();
                    (l, s)
                }),
            });
        }
        Ok(ExprInputs { fans })
    }

    /// Evaluates fan `i` at `now` (ms). Sample times are returned in seconds.
    pub fn eval(&mut self, i: usize, src: &dyn SensorSource, now: TimestampMs) -> FanInputs {
        let f = &mut self.fans[i];
        let conv = |r: Result<orca_sensors::Value, orca_sensors::Missing>| {
            r.ok().map(|v| Sample {
                value: v.v,
                ts: v.ts.unwrap_or(now) as f64 / 1000.0,
            })
        };
        let temp = conv(f.temp.0.eval(src, now, &mut f.temp.1));
        let load = f.load.as_mut().and_then(|(e, s)| conv(e.eval(src, now, s)));
        FanInputs { temp, load }
    }
}
