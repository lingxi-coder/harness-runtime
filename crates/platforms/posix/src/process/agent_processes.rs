//! Per-agent live process groups (`Dbr`/`YOe`), independent of task records.
pub(super) use lingxi_core::host::agent_processes::register;

#[cfg(test)]
mod tests {
    use super::super::runner::PosixProcess;
    use super::*;
    use lingxi_core::host::ProcessRunner;
    #[tokio::test]
    async fn owner_scoped_group_kill_excludes_other_agents_and_unregisters() {
        fn spawn() -> tokio::process::Child {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.args(["-c", "sleep 60 & wait"]).kill_on_drop(true);
            super::super::spawn_unsafe::attach_setsid(&mut cmd);
            cmd.spawn().unwrap()
        }
        let mut owned = spawn();
        let mut other = spawn();
        let a = format!("owner-{}", owned.id().unwrap());
        let b = format!("other-{}", other.id().unwrap());
        let registration = register(Some(&a), owned.id());
        let other_registration = register(Some(&b), other.id());
        assert_eq!(
            PosixProcess::new().kill_owner_processes(&a).await,
            vec![owned.id().unwrap()]
        );
        assert!(!owned.wait().await.unwrap().success());
        assert!(other.try_wait().unwrap().is_none());
        drop(registration);
        assert!(PosixProcess::new()
            .kill_owner_processes(&a)
            .await
            .is_empty());
        PosixProcess::new().kill_owner_processes(&b).await;
        other.wait().await.unwrap();
        drop(other_registration);
        assert!(PosixProcess::new()
            .kill_owner_processes(&b)
            .await
            .is_empty());
    }
}
