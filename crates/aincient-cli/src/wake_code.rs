//! `engine wake-code` on every platform: the host-side `engine` module is
//! unix-only, but a paired or SSH site's wake code is managed from Windows too.

use aincient_core::engine_admin;
use anyhow::Result;

/// `engine wake-code` through `docker exec` (this computer or SSH).
pub fn wake_code_admin<D: engine_admin::DockerExec + ?Sized>(
    docker: &D,
    info: Option<&engine_admin::EngineInfo>,
    change: Option<bool>,
) -> Result<()> {
    match change {
        None => {
            let st = engine_admin::EngineAdmin::new(docker, info, "wake-code")?.status()?;
            println!("{}", wake_code_state(st.wake_code_set, st.lockdown));
        }
        Some(true) => {
            let code =
                engine_admin::wake_code(docker, info, false)?.expect("a rotate returns its code");
            println!("{}", engine_admin::wake_code_notice(&code));
        }
        Some(false) => {
            engine_admin::wake_code(docker, info, true)?;
            println!("Wake code revoked: waking by code is refused until you make a new one.");
        }
    }
    Ok(())
}

/// Whether a wake code is set, in words (never the code: only its hash is kept).
pub fn wake_code_state(set: bool, locked: bool) -> String {
    let state = if set {
        "A wake code is set (only its hash is kept; `--rotate` makes a new one)."
    } else {
        "No wake code: waking by code is refused. `--rotate` makes one."
    };
    if locked {
        format!("{state} The site is locked down, so waking by code is off until unlock.")
    } else {
        state.to_string()
    }
}
