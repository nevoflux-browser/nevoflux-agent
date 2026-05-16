#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub run_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EvalConfigError {
    #[error(
        "NEVOFLUX_EVAL_MODE=1 but NEVOFLUX_EVAL_RUN_ID not set. \
         Eval mode requires both env vars. \
         Set NEVOFLUX_EVAL_RUN_ID to a unique identifier (e.g. run-YYYYMMDD-HHMMSS)."
    )]
    MissingRunId,
}

pub fn from_env() -> Result<Option<EvalConfig>, EvalConfigError> {
    if std::env::var("NEVOFLUX_EVAL_MODE").ok().as_deref() != Some("1") {
        return Ok(None);
    }
    let run_id =
        std::env::var("NEVOFLUX_EVAL_RUN_ID").map_err(|_| EvalConfigError::MissingRunId)?;
    if run_id.trim().is_empty() {
        return Err(EvalConfigError::MissingRunId);
    }
    Ok(Some(EvalConfig { run_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(*k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, prev_val) in prev {
            match prev_val {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn eval_mode_off_returns_none() {
        with_env(
            &[("NEVOFLUX_EVAL_MODE", None), ("NEVOFLUX_EVAL_RUN_ID", None)],
            || {
                assert!(from_env().unwrap().is_none());
            },
        );
    }

    #[test]
    fn eval_mode_set_without_run_id_errors() {
        with_env(
            &[
                ("NEVOFLUX_EVAL_MODE", Some("1")),
                ("NEVOFLUX_EVAL_RUN_ID", None),
            ],
            || {
                let err = from_env().unwrap_err();
                assert!(matches!(err, EvalConfigError::MissingRunId));
            },
        );
    }

    #[test]
    fn eval_mode_set_with_empty_run_id_errors() {
        with_env(
            &[
                ("NEVOFLUX_EVAL_MODE", Some("1")),
                ("NEVOFLUX_EVAL_RUN_ID", Some("   ")),
            ],
            || {
                assert!(matches!(
                    from_env().unwrap_err(),
                    EvalConfigError::MissingRunId
                ));
            },
        );
    }

    #[test]
    fn well_formed_returns_some() {
        with_env(
            &[
                ("NEVOFLUX_EVAL_MODE", Some("1")),
                ("NEVOFLUX_EVAL_RUN_ID", Some("run-20260516-100000")),
            ],
            || {
                let cfg = from_env().unwrap().expect("expected Some");
                assert_eq!(cfg.run_id, "run-20260516-100000");
            },
        );
    }
}
