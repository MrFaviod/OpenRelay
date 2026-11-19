//! Pruebas de los fallbacks de F7: PulseAudio por proceso y ALSA opt-in.
//!
//! Las pruebas de PulseAudio usan un ejecutor falso que registra cada llamada y
//! mantiene un conjunto de módulos; la prueba de integración levanta una
//! instancia privada del binario `pulseaudio` y comprueba que la lista de
//! módulos vuelve exactamente al estado inicial.
//!
//! Las pruebas de ALSA no abren hardware: verifican que sin opt-in no hay
//! apertura y que cualquier nombre que no sea `hw:` se rechaza. Este módulo no
//! ejecuta procesos, así que no hay `modprobe` que probar en tiempo de
//! ejecución; la garantía es estructural.

#![cfg(any(feature = "pulse", feature = "alsa"))]

#[cfg(feature = "pulse")]
mod pulse_fallback {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use openrelay_audio_linux::pulse::{
        Pactl, PactlProcess, PulseError, PulseFallback, SINK_NAME, SOURCE_NAME,
    };

    #[derive(Debug, Default)]
    struct FakeState {
        calls: Vec<Vec<String>>,
        modules: BTreeSet<u32>,
        next_id: u32,
        load_calls: usize,
        fail_load_at: Option<usize>,
        fail_unload: bool,
        info: String,
    }

    type Shared = Rc<RefCell<FakeState>>;

    #[derive(Clone, Debug)]
    struct FakePactl(Shared);

    impl FakePactl {
        fn new(server_name: &str) -> Self {
            Self(Rc::new(RefCell::new(FakeState {
                info: format!("Server Name: {server_name}\n"),
                next_id: 100,
                ..FakeState::default()
            })))
        }

        fn with_foreign_module(self, id: u32) -> Self {
            self.0.borrow_mut().modules.insert(id);
            self
        }

        fn fail_load_at(&self, call: usize) {
            self.0.borrow_mut().fail_load_at = Some(call);
        }

        fn fail_unload(&self) {
            self.0.borrow_mut().fail_unload = true;
        }

        fn modules(&self) -> BTreeSet<u32> {
            self.0.borrow().modules.clone()
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.0.borrow().calls.clone()
        }
    }

    impl Pactl for FakePactl {
        fn run(&mut self, args: &[&str]) -> Result<String, PulseError> {
            let mut state = self.0.borrow_mut();
            state
                .calls
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            match args {
                ["info"] => Ok(state.info.clone()),
                ["load-module", ..] => {
                    state.load_calls += 1;
                    if state.fail_load_at == Some(state.load_calls) {
                        return Err(PulseError::Exit {
                            args: args.join(" "),
                            code: Some(1),
                            stderr: "fallo simulado".to_string(),
                        });
                    }
                    let id = state.next_id;
                    state.next_id += 1;
                    state.modules.insert(id);
                    Ok(id.to_string())
                }
                ["unload-module", raw_id] => {
                    let id: u32 = raw_id.parse().expect("índice de módulo");
                    if state.fail_unload {
                        return Err(PulseError::Exit {
                            args: args.join(" "),
                            code: Some(1),
                            stderr: "descarga simulada".to_string(),
                        });
                    }
                    state.modules.remove(&id);
                    Ok(String::new())
                }
                _ => Err(PulseError::Parse {
                    what: "fake",
                    output: args.join(" "),
                }),
            }
        }
    }

    fn unload_calls(calls: &[Vec<String>]) -> Vec<u32> {
        calls
            .iter()
            .filter(|call| call.first().map(String::as_str) == Some("unload-module"))
            .map(|call| call[1].parse().expect("índice descargado"))
            .collect()
    }

    #[test]
    fn unloads_only_owned_modules_and_leaves_foreign_ones() {
        let fake = FakePactl::new("pulseaudio").with_foreign_module(7);
        let fallback = PulseFallback::activate(fake.clone())
            .expect("activación")
            .expect("PulseAudio nativo debe activar el fallback");

        assert_eq!(fallback.module_ids().to_vec(), vec![100, 101]);
        assert_eq!(fallback.source_name(), SOURCE_NAME);
        assert_eq!(fake.modules(), BTreeSet::from([7, 100, 101]));

        drop(fallback);

        assert_eq!(
            fake.modules(),
            BTreeSet::from([7]),
            "el módulo ajeno 7 debe seguir cargado"
        );
        assert_eq!(
            unload_calls(&fake.calls()),
            vec![101, 100],
            "sólo se descargan los índices propios, en orden inverso"
        );
    }

    #[test]
    fn does_not_activate_on_pipewire() {
        let fake = FakePactl::new("PulseAudio (on PipeWire 1.0.5)");
        let activated = PulseFallback::activate(fake.clone()).expect("consulta");

        assert!(
            activated.is_none(),
            "PipeWire nunca crea módulos PulseAudio"
        );
        assert_eq!(
            fake.calls(),
            vec![vec!["info".to_string()]],
            "sólo se consulta el servidor"
        );
    }

    #[test]
    fn cleans_up_first_module_when_second_load_fails() {
        let fake = FakePactl::new("pulseaudio").with_foreign_module(7);
        fake.fail_load_at(2);

        let error = PulseFallback::activate(fake.clone()).expect_err("la segunda carga falla");

        assert!(matches!(error, PulseError::Exit { .. }));
        assert_eq!(
            fake.modules(),
            BTreeSet::from([7]),
            "el sink nulo propio se descarga y el ajeno permanece"
        );
        assert_eq!(unload_calls(&fake.calls()), vec![100]);
    }

    #[test]
    fn close_reports_unload_error_and_does_not_retry() {
        let fake = FakePactl::new("pulseaudio");
        let fallback = PulseFallback::activate(fake.clone())
            .expect("activación")
            .expect("PulseAudio nativo");
        fake.fail_unload();

        let error = fallback.close().expect_err("la descarga falla");

        assert!(matches!(error, PulseError::Exit { .. }));
        assert_eq!(
            unload_calls(&fake.calls()),
            vec![101, 100],
            "se intentan ambos índices una sola vez"
        );
    }

    /// Binario que usa la prueba de integración; si no existe, la prueba se
    /// salta con un mensaje y no se acredita nada.
    const PULSEAUDIO: &str = "/usr/bin/pulseaudio";

    struct TempTree(PathBuf);

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("reloj del sistema")
            .as_nanos();
        std::env::temp_dir().join(format!("openrelay-f7-pulse-{}-{nanos}", std::process::id()))
    }

    fn wait_for_socket(path: &Path) -> bool {
        for _ in 0..200 {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn pactl_modules(server: &str, home: &Path) -> String {
        let output = Command::new("pactl")
            .args(["list", "short", "modules"])
            .env("PULSE_SERVER", server)
            .env("HOME", home)
            .output()
            .expect("pactl debe estar disponible con la feature pulse");
        assert!(
            output.status.success(),
            "pactl list short modules falló: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn private_server_loads_unloads_and_restores_initial_state() {
        if !Path::new(PULSEAUDIO).exists() {
            eprintln!("SKIP: {PULSEAUDIO} no existe; prueba de integración PulseAudio omitida");
            return;
        }

        let root = temp_root();
        let runtime = root.join("run");
        let home = root.join("home");
        std::fs::create_dir_all(runtime.join("pulse")).expect("runtime dir");
        std::fs::create_dir_all(&home).expect("home temporal");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
                .expect("permisos 0700 del runtime");
        }
        let _tree = TempTree(root);

        let socket = runtime.join("pulse").join("native");
        let child = Command::new(PULSEAUDIO)
            .args([
                "--daemonize=no",
                "-n",
                "--use-pid-file=no",
                "--exit-idle-time=15",
                "--log-target=stderr",
                "--log-level=error",
            ])
            .arg("-L")
            .arg(format!(
                "module-native-protocol-unix socket={} auth-anonymous=1",
                socket.display()
            ))
            .arg("-L")
            .arg("module-null-sink sink_name=baseline")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("HOME", &home)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("no se pudo lanzar la instancia privada de pulseaudio");
        let _child = ChildGuard(child);

        assert!(
            wait_for_socket(&socket),
            "la instancia privada no creó el socket {}",
            socket.display()
        );
        let server = format!("unix:{}", socket.display());
        let before = pactl_modules(&server, &home);
        assert!(
            before.contains("sink_name=baseline"),
            "el estado inicial debe tener el sink base: {before}"
        );

        let fallback = PulseFallback::activate(PactlProcess::with_pulse_server(server.clone()))
            .expect("activación contra la instancia privada")
            .expect("PulseAudio nativo debe activar el fallback");
        assert_eq!(fallback.module_ids().len(), 2);

        let during = pactl_modules(&server, &home);
        assert!(
            during.contains(SINK_NAME),
            "el sink propio debe estar cargado: {during}"
        );
        assert!(
            during.contains(SOURCE_NAME),
            "la fuente propia debe estar publicada: {during}"
        );

        fallback.close().expect("descarga de los módulos propios");

        let after = pactl_modules(&server, &home);
        assert_eq!(
            after, before,
            "la lista de módulos debe volver exactamente al estado inicial"
        );
    }
}

#[cfg(feature = "alsa")]
mod alsa_opt_in {
    use openrelay_audio_linux::alsa::{open_from_config, AlsaConfig, AlsaError, AlsaHwSource};

    #[test]
    fn stays_closed_without_opt_in() {
        let config = AlsaConfig::disabled();

        assert!(
            open_from_config(&config)
                .expect("sin configuración")
                .is_none(),
            "sin hw_device no se abre ningún PCM"
        );
    }

    #[test]
    fn rejects_non_hw_devices_without_implicit_module_loading() {
        for device in ["default", "plughw:0,0", "sysdefault", "pulse"] {
            let error = AlsaHwSource::open(device).expect_err("no debe abrir");
            assert!(
                matches!(error, AlsaError::NotHwDevice(_)),
                "{device} debe rechazarse antes de tocar libasound"
            );
        }

        let configured = AlsaConfig::hw("default");
        let error = open_from_config(&configured).expect_err("no debe abrir");
        assert!(matches!(error, AlsaError::NotHwDevice(_)));
    }
}
