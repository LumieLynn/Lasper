use super::*;

struct Manager;
#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let path = match name {
            "fixture.scope" => "/org/freedesktop/systemd1/unit/fixture_scope",
            "fixture.service" => "/org/freedesktop/systemd1/unit/fixture_service",
            _ => return Err(zbus::fdo::Error::Failed("unexpected hosting unit".into())),
        };
        Ok(OwnedObjectPath::try_from(path).unwrap())
    }
}
struct Scope;
#[zbus::interface(name = "org.freedesktop.systemd1.Scope")]
impl Scope {
    #[zbus(property)]
    fn control_group(&self) -> &str {
        "/machine.slice/fixture.scope"
    }
}
struct Service;
#[zbus::interface(name = "org.freedesktop.systemd1.Service")]
impl Service {
    #[zbus(property)]
    fn control_group(&self) -> &str {
        "/machine.slice/fixture.service"
    }
}

#[tokio::test]
async fn control_group_uses_the_hosting_units_type_specific_interface() {
    timeout(QUERY_TIMEOUT, async {
        // Exercise the actual GetUnit + Properties.Get wire requests. The
        // fixture exposes ControlGroup only on its real systemd interface;
        // querying generic Unit must fail, just as it does on a host.
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let server = zbus::connection::Builder::unix_stream(server)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .serve_at("/org/freedesktop/systemd1", Manager)
            .unwrap()
            .serve_at("/org/freedesktop/systemd1/unit/fixture_scope", Scope)
            .unwrap()
            .serve_at("/org/freedesktop/systemd1/unit/fixture_service", Service)
            .unwrap()
            .name(SYSTEMD)
            .unwrap()
            .build();
        let client = zbus::connection::Builder::unix_stream(client).p2p().build();
        let (server, client) = tokio::try_join!(server, client).unwrap();
        let source = HostMachines(client);
        assert_eq!(
            source.control_group("fixture.scope").await.unwrap(),
            PathBuf::from("/machine.slice/fixture.scope")
        );
        assert_eq!(
            source.control_group("fixture.service").await.unwrap(),
            PathBuf::from("/machine.slice/fixture.service")
        );
        assert_eq!(
            source
                .control_group("fixture.slice")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        drop(server);
    })
    .await
    .expect("host cgroup D-Bus fixture timed out");
}

#[test]
fn proc_stat_comm_may_contain_parentheses_and_spaces() {
    let fields = format!("S {} 12345 0", "0 ".repeat(18));
    assert_eq!(
        start_time(format!("42 (name ) with spaces) {fields}").as_bytes())
            .unwrap()
            .get(),
        12345
    );
    let mut non_utf8 = b"42 (\xff) name) ".to_vec();
    non_utf8.extend_from_slice(fields.as_bytes());
    assert_eq!(start_time(&non_utf8).unwrap().get(), 12345);
    assert!(start_time(b"42 (broken)").is_err());
}
#[test]
fn real_proc_start_time_is_nonzero() {
    assert!(start_time(&std::fs::read("/proc/self/stat").unwrap()).is_ok());
}
