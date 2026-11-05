//! The bench file's `fixture()` (SPEC §9.10): a process both sides of a comparison talk to (a server
//! with seeded data), started by strobe for every block in a group of its own and a home of its own,
//! so it is in neither side's process set, and ended with the block. Its CPU per run is `cpu.fixture`
//! (what each side made it do); a block whose app never connected to it, or ran its executable itself
//! (an app that started its own server), is refused `FixtureMismatch`.

use super::*;
use std::rc::Rc;

/// What `fixture(command, { seed, env, ready, timeout })` declared.
pub(super) struct Spec {
    command: String,
    /// The seed folder, relative to the bench file (`{param}` placeholders allowed): copied into the
    /// fixture's home for every block.
    seed: Option<String>,
    env: Vec<(String, String)>,
    /// A URL that answers once the fixture is ready (`{port}` filled), else the port accepting.
    ready: Option<String>,
    timeout: Duration,
    /// The bench file's folder: the command runs there and the seed is found from there.
    dir: PathBuf,
}

/// The `{key}` placeholders of a fixture's command, env and ready URL: `{port}`, `{home}` and the
/// series' params.
fn fill(text: &str, port: u16, home: &Path, params: &BTreeMap<String, Value>) -> String {
    let mut s = text.replace("{port}", &port.to_string()).replace("{home}", &home.display().to_string());
    for (k, v) in params {
        s = s.replace(&format!("{{{k}}}"), &value_text(v));
    }
    s
}

impl Spec {
    /// The bench file's fixture, if it declares one.
    pub(super) fn from(describe: &Value) -> Result<Option<Spec>> {
        let f = &describe["fixture"];
        if !f.is_object() {
            return Ok(None);
        }
        let command = f["command"].as_str().filter(|c| !c.trim().is_empty()).ok_or_else(|| Error::usage("fixture(): give the command that starts it, with {port} where it listens"))?;
        if !command.contains("{port}") && f["port"].as_u64().is_none() {
            return Err(Error::usage(format!("fixture(\"{command}\"): put {{port}} where it listens (strobe gives it a free port per block)")));
        }
        Ok(Some(Spec {
            command: command.to_string(),
            seed: f["seed"].as_str().map(str::to_string),
            env: f["env"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect(),
            ready: f["ready"].as_str().map(str::to_string),
            timeout: Duration::from_millis(f["timeout"].as_u64().unwrap_or(60_000)),
            dir: Path::new(describe["file"].as_str().unwrap_or(".")).parent().unwrap_or(Path::new(".")).to_path_buf(),
        }))
    }

    /// The seed folder for these params.
    fn seed_dir(&self, params: &BTreeMap<String, Value>) -> Option<PathBuf> {
        self.seed.as_ref().map(|s| self.dir.join(fill(s, 0, Path::new(""), params)))
    }

    /// Starts the fixture for one block: a fresh home with the seed copied in, a free port, its own
    /// process group on `cpus` (the CPUs the target is not pinned to: given, not inherited, since the
    /// daemon moves only its bench's thread there), and waits until it answers.
    pub(super) fn start(&self, params: &BTreeMap<String, Value>, log: &Path, cpus: Option<u64>) -> Result<Rc<Running>> {
        let home = isolate::new_home();
        std::fs::create_dir_all(&home)?;
        if let Some(seed) = self.seed_dir(params) {
            if !seed.is_dir() {
                let _ = std::fs::remove_dir_all(&home);
                return Err(Error::usage(format!("fixture seed {} is no folder", seed.display())));
            }
            for f in walk(&seed) {
                let to = home.join(f.strip_prefix(&seed).unwrap_or(&f));
                if let Some(p) = to.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(&f, &to)?;
            }
        }
        let port = free_port()?;
        let cmd = fill(&self.command, port, &home, params);
        let mut c = shell(&cmd);
        c.current_dir(&self.dir).stdin(Stdio::null());
        for (k, v) in isolate::launch_env(&home).into_iter().chain(self.env.iter().map(|(k, v)| (k.clone(), fill(v, port, &home, params)))) {
            c.env(k, v);
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(log) {
            Ok(f) => {
                let e = f.try_clone()?;
                c.stdout(f).stderr(e);
            }
            Err(_) => {
                c.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let mut owned = host::spawn_owned(&mut c, cpus, 0, Some(&home)).map_err(|e| Error::msg(format!("fixture: {cmd}: {e}")))?;
        let pid = owned.pid();
        let ready = self.ready.as_deref().map(|r| fill(r, port, &home, params));
        let deadline = Instant::now() + self.timeout;
        loop {
            let up = match &ready {
                Some(url) => http_ok(url),
                None => std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok(),
            };
            if up {
                break;
            }
            if let Some(code) = owned.exited() {
                isolate::release_home(&home);
                let _ = std::fs::remove_dir_all(&home);
                return Err(Error::msg(format!("the fixture exited (code {code}) before it was ready: {cmd} (its output: {})", log.display())));
            }
            if Instant::now() > deadline {
                drop(owned);
                isolate::release_home(&home);
                let _ = std::fs::remove_dir_all(&home);
                return Err(Error::timeout(format!(
                    "the fixture was not ready in {} s: {cmd} (waited for {}; its output: {})",
                    self.timeout.as_secs(),
                    ready.as_deref().unwrap_or("its port"),
                    log.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // What it runs, beyond the shell that started it: a side's process that runs one of these is
        // an app that started its own copy. A script runtime (node, bun, python, …) runs anything, so
        // a side running one says nothing: an interpreted fixture gets no such check.
        let shells = ["cmd", "sh", "bash", "zsh", "dash", "pwsh", "powershell", "fish"];
        let exes: Vec<PathBuf> = host::process_tree(pid)
            .iter()
            .filter_map(|p| host::exe_of(p.pid))
            .filter(|e| !shells.contains(&e.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default().as_str()) && !isolate::interpreter(e))
            .collect();
        Ok(Rc::new(Running { owned: RefCell::new(Some(owned)), port, pid, home, exes, connected: Cell::new(false), checked: Cell::new(false), own_copy: RefCell::new(None) }))
    }

    /// What result.json says of the fixture: its command, seed and the seed's hash, whether every block
    /// of each side was connected to it, and its CPU per run per side (median over the blocks).
    pub(super) fn summary(&self, series: &[Value]) -> Value {
        use sha2::Digest;
        let mut seeds: Vec<PathBuf> = series.iter().filter_map(|s| self.seed_dir(&serde_json::from_value(s["params"].clone()).unwrap_or_default())).collect();
        seeds.sort();
        seeds.dedup();
        let hash = (!seeds.is_empty()).then(|| {
            let mut h = sha2::Sha256::new();
            for seed in &seeds {
                let mut files = walk(seed);
                files.sort();
                for f in files {
                    h.update(f.strip_prefix(seed).unwrap_or(&f).to_string_lossy().replace('\\', "/").as_bytes());
                    h.update(std::fs::read(&f).unwrap_or_default());
                }
            }
            format!("{:x}", h.finalize())
        });
        let blocks = |role: &str| -> Vec<Value> { series.iter().flat_map(|s| s["blocks"].as_array().cloned().unwrap_or_default()).filter(|b| b["variant"] == role).collect() };
        let side = |role: &str| {
            let bs = blocks(role);
            let checked: Vec<bool> = bs.iter().filter_map(|b| b["fixture_connected"].as_bool()).collect();
            let cpu: Vec<f64> = bs.iter().filter_map(|b| b["values"]["cpu.fixture"].as_f64()).collect();
            (if checked.is_empty() { Value::Null } else { json!(checked.iter().all(|c| *c)) }, if cpu.is_empty() { Value::Null } else { json!(metrics::median(&cpu)) })
        };
        let (cb, pb) = side("base");
        let (ch, ph) = side("head");
        json!({
            "command": self.command, "seed": self.seed, "seed_sha256": hash,
            "connected": { "base": cb, "head": ch }, "cpu_ms_per_run": { "base": pb, "head": ph },
        })
    }
}

/// One block's fixture: ended (its group and its home) when the block lets go of it.
pub(super) struct Running {
    owned: RefCell<Option<host::spawn::Owned>>,
    pub(super) port: u16,
    pub(super) pid: u32,
    home: PathBuf,
    exes: Vec<PathBuf>,
    /// A side's process held a connection to it at a run's start or end.
    connected: Cell<bool>,
    /// The connection was read at least once (a launch case reads none).
    checked: Cell<bool>,
    /// A side's process that runs the fixture's executable: (pid, executable).
    own_copy: RefCell<Option<(u32, PathBuf)>>,
}

impl Running {
    pub(super) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `{fixture.url}`, `{fixture.port}` and `{fixture.pid}` filled into every string of a target's
    /// options (its args, env, files, url).
    pub(super) fn fill(&self, v: &Value) -> Value {
        match v {
            Value::String(s) if s.contains("{fixture.") => {
                // A JSON file's number stays a number.
                match s.as_str() {
                    "{fixture.port}" => return json!(self.port),
                    "{fixture.pid}" => return json!(self.pid),
                    _ => {}
                }
                json!(s.replace("{fixture.url}", &self.url()).replace("{fixture.port}", &self.port.to_string()).replace("{fixture.pid}", &self.pid.to_string()))
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.fill(x)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), self.fill(x))).collect()),
            other => other.clone(),
        }
    }

    /// The fixture's CPU so far: its tree's cycles and the cycle clock.
    pub(super) fn cpu(&self) -> (u64, Option<u64>) {
        super::script::cpu(self.pid)
    }

    /// Reads which of a side's processes hold a connection to the fixture, and whether one runs the
    /// fixture's own executable. Outside every timed span.
    pub(super) fn check(&self, side: &[u32]) {
        if let Some(held) = host::tcp::connected(self.port, side) {
            self.checked.set(true);
            if !held.is_empty() {
                self.connected.set(true);
            }
        }
        if self.own_copy.borrow().is_none()
            && let Some(hit) = side.iter().find_map(|&p| host::exe_of(p).filter(|e| self.exes.iter().any(|x| same_file(x, e))).map(|e| (p, e)))
        {
            *self.own_copy.borrow_mut() = Some(hit);
        }
    }

    /// `FixtureMismatch` for a block whose side never connected to the fixture, or ran its executable.
    /// Some(connected) when it was read, None when the block read nothing (a launch case).
    pub(super) fn verdict(&self, side: &str) -> Result<Option<bool>> {
        if let Some((pid, exe)) = self.own_copy.borrow().clone() {
            return Err(Error::refused(
                "FixtureMismatch",
                format!("{side}'s process {pid} runs the fixture's own executable ({}): the app started a server of its own instead of using the fixture", exe.display()),
                "point the app at the fixture ({fixture.url} in the target's args, env or files) so it does not start its own",
            ));
        }
        if self.checked.get() && !self.connected.get() {
            return Err(Error::refused(
                "FixtureMismatch",
                format!("none of {side}'s processes held a connection to the fixture (port {}) after setup, at a run's start or end, or once its launch was ready", self.port),
                "point the app at the fixture: {fixture.url} or {fixture.port} in the target's args, env or files",
            ));
        }
        Ok(self.checked.get().then_some(self.connected.get()))
    }
}

/// Two paths name one file (case and separators folded on Windows).
fn same_file(a: &Path, b: &Path) -> bool {
    host::spawn::home_key(a) == host::spawn::home_key(b)
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut o) = self.owned.borrow_mut().take() {
            o.end();
        }
        isolate::release_home(&self.home);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
