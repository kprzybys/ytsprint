//! ytsprint-jsd (src/jsd.js) with a real deno and stub ejs scripts: protocol, preparing a player,
//! a damaged prepared player, and the sandbox (the player code cannot read files).
//! Needs deno in PATH (or YTSPRINT_DENO).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

// Stub ejs: "preparing" a player returns the player source unchanged; a prepared player is expected
// to set _result.n / _result.sig (like the real ejs output).
const LIB: &str = "var lib = {};";
const CORE: &str = r#"var jsc = function (input) {
  return { type: "result", responses: [], preprocessed_player: input.player };
};"#;
const PLAYER: &str = r#"_result.n = (x) => x.split("").reverse().join(""); _result.sig = (x) => x + "!";"#;

struct Jsd {
    child: Child,
    sock: PathBuf,
    _dir: tempdir::Dir,
}

impl Drop for Jsd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn deno() -> PathBuf {
    if let Ok(d) = std::env::var("YTSPRINT_DENO") {
        return d.into();
    }
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("deno")).find(|d| d.is_file()))
        .expect("deno not found in PATH (this test needs it; set YTSPRINT_DENO)")
}

fn start() -> Jsd {
    let dir = std::env::temp_dir().join(format!(
        "ytsprint-jsd-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("jsd.sock");
    let s = sock.to_str().unwrap();
    let child = Command::new(deno())
        .args([
            "run",
            "--no-prompt",
            "--no-config",
            "--no-lock",
            "--no-npm",
            "--no-remote",
        ])
        .arg(format!("--allow-net=unix:{s}"))
        .arg(format!("--allow-read={s}"))
        .arg(format!("--allow-write={s}"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/jsd.js"))
        .arg(s)
        .spawn()
        .expect("start deno");
    let t = Instant::now();
    while UnixStream::connect(&sock).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "jsd did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Jsd {
        child,
        sock,
        _dir: tempdir::Dir(dir),
    }
}

fn ask(jsd: &Jsd, req: &Value) -> Value {
    let mut c = UnixStream::connect(&jsd.sock).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn base(key: &str) -> Value {
    json!({"player_key": key, "ejs_tag": "0123abcd", "requests": [
        {"type": "n", "challenges": ["abc"]}, {"type": "sig", "challenges": ["s1"]}
    ]})
}

#[test]
fn protocol_prepare_and_solve() {
    let jsd = start();
    let key = "1234abcd-ejs0123abcd";

    // a fresh jsd knows neither ejs nor the player
    let v = ask(&jsd, &base(key));
    assert_eq!(v["type"], "need", "{v}");
    assert_eq!(v["what"], json!(["ejs", "player"]));

    // with ejs and the raw player: prepared, solved, prepared player returned for the client to cache
    let mut req = base(key);
    req["ejs"] = json!({"lib": LIB, "core": CORE});
    req["raw"] = json!(PLAYER);
    let v = ask(&jsd, &req);
    assert_eq!(v["type"], "result", "{v}");
    assert_eq!(v["responses"][0]["data"]["abc"], "cba");
    assert_eq!(v["responses"][1]["data"]["s1"], "s1!");
    assert_eq!(v["preprocessed_player"], PLAYER);

    // now it is in memory: the small request is enough, nothing to cache
    let v = ask(&jsd, &base(key));
    assert_eq!(v["type"], "result", "{v}");
    assert!(v.get("preprocessed_player").is_none());

    // the client lost its cached copy and sends the raw player again: it gets the prepared one back
    let mut req = base(key);
    req["raw"] = json!(PLAYER);
    assert_eq!(ask(&jsd, &req)["preprocessed_player"], PLAYER);

    // another player with known ejs: only the player is needed; a prepared one is accepted as is
    let other = "5678abcd-ejs0123abcd";
    assert_eq!(ask(&jsd, &base(other))["what"], json!(["player"]));
    let mut req = base(other);
    req["pre"] = json!(PLAYER);
    assert_eq!(ask(&jsd, &req)["responses"][0]["data"]["abc"], "cba");
}

#[test]
fn damaged_prepared_player_is_reported() {
    let jsd = start();
    let mut req = base("1234abcd-ejs0123abcd");
    req["ejs"] = json!({"lib": LIB, "core": CORE});
    req["pre"] = json!("_result.n = (x) => {"); // truncated file
    let v = ask(&jsd, &req);
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["kind"], "bad_prepared");
}

#[test]
fn player_code_cannot_read_files() {
    let jsd = start();
    let mut req = base("1234abcd-ejs0123abcd");
    req["ejs"] = json!({"lib": LIB, "core": CORE});
    req["pre"] = json!(r#"Deno.readTextFileSync("/etc/passwd"); _result.n = (x) => x;"#);
    let v = ask(&jsd, &req);
    assert_eq!(v["type"], "error", "{v}");
    assert!(v["error"].as_str().unwrap().contains("NotCapable"), "{v}");
}

#[test]
fn bad_requests_are_rejected() {
    let jsd = start();
    let v = ask(
        &jsd,
        &json!({"player_key": "../../etc", "ejs_tag": "0123abcd", "requests": []}),
    );
    assert_eq!(v["kind"], "protocol", "{v}");
    let mut c = UnixStream::connect(&jsd.sock).unwrap();
    c.write_all(b"not json\n").unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    assert!(line.contains("\"protocol\""), "{line}");
}
