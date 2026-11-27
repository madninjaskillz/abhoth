//! `abhoth-nodens run|check --config FILE`: Nodens, the supervisor, on its own. The same program
//! as `abhoth nodens` and the old `abhoth-supervisor` (docs/NAMES.md).

fn main() {
    orca_supervisor::run::standalone();
}
