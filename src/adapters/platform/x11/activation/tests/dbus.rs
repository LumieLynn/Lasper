use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{take_property, UserManager, DESTINATION, MANAGER_PATH};
use crate::adapters::platform::x11::activation::{
    ActivationSpec, UnitManager, COMMAND_TIMEOUT, PATH_UNIT, SERVICE_UNIT,
};

type Properties = Vec<(String, OwnedValue)>;
type Auxiliary = Vec<(String, Properties)>;

struct Request {
    name: String,
    mode: String,
    properties: Properties,
    auxiliary: Auxiliary,
}

struct Manager {
    requests: Arc<Mutex<Vec<Request>>>,
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    async fn start_transient_unit(
        &self,
        name: String,
        mode: String,
        properties: Properties,
        auxiliary: Auxiliary,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let (id, result) = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(Request {
                name: name.clone(),
                mode,
                properties,
                auxiliary,
            });
            let id = requests.len() as u32;
            (id, if id == 1 { "done" } else { "failed" })
        };
        let job = OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{id}")).unwrap();
        // Emit before the method reply to exercise immediately completed jobs.
        Self::job_removed(&emitter, id, &job, &name, result).await?;
        Ok(job)
    }

    #[zbus(signal)]
    async fn job_removed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        path: &OwnedObjectPath,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}

#[tokio::test]
#[ignore = "requires dbus-daemon; uses an isolated session bus"]
async fn transient_registration_uses_systemd_wire_types_and_checks_the_job_result() {
    let mut bus = tokio::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut address = String::new();
    tokio::time::timeout(
        COMMAND_TIMEOUT,
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let _server = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .name(DESTINATION)
        .unwrap()
        .serve_at(
            MANAGER_PATH,
            Manager {
                requests: requests.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let manager = UserManager {
        connection: zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap(),
    };
    let spec = ActivationSpec {
        executable: "/opt/Lasper builds/$binary/lasper".into(),
        watched_directory: "/run/systemd/machines".into(),
    };
    manager.create(&spec, true).await.unwrap();
    let error = manager.create(&spec, false).await.unwrap_err();
    assert!(error.contains("job finished with failed"), "{error}");
    {
        let mut requests = requests.lock().unwrap();
        assert!(requests.pop().unwrap().auxiliary.is_empty());
        let request = requests.pop().unwrap();
        assert_eq!(request.name, PATH_UNIT);
        assert_eq!(request.mode, "fail");
        let mut properties: HashMap<_, _> = request.properties.into_iter().collect();
        let paths: Vec<(String, String)> = properties.remove("Paths").unwrap().try_into().unwrap();
        assert_eq!(
            paths,
            [("PathChanged".into(), "/run/systemd/machines".into())]
        );
        assert_eq!(request.auxiliary.len(), 1);
        let (name, service) = request.auxiliary.into_iter().next().unwrap();
        assert_eq!(name, SERVICE_UNIT);
        let mut service: HashMap<_, _> = service.into_iter().collect();
        assert_eq!(take_property::<u32>(&mut service, "UMask").unwrap(), 0o077);
        assert_eq!(
            take_property::<String>(&mut service, "Type").unwrap(),
            "oneshot"
        );
        let commands: Vec<(String, Vec<String>, bool)> =
            service.remove("ExecStart").unwrap().try_into().unwrap();
        assert_eq!(
            commands,
            [(
                spec.executable.clone(),
                vec![
                    "/opt/Lasper builds/$$binary/lasper".into(),
                    "--internal-x11-reconcile".into(),
                ],
                false
            )]
        );
    }
    bus.kill().await.unwrap();
    bus.wait().await.unwrap();
}
