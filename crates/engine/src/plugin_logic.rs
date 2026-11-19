//! Logic plugins (OBS-COMPETE.md §4, kind 2): WebAssembly run by `wasmi`, an interpreter, in a
//! sandbox. A plugin gets only what we hand it: no files, no network, no clock, no keys. It is
//! told about events as small JSON (who, what, how much: never ids or tokens) and can ask for
//! the steps "When X, do Y" has, only those its `plugin.json` lists (shown at install).
//!
//! The module's side (any language that builds to `wasm32`):
//! - exports `memory`, `alloc(len: i32) -> i32` (room for an event) and
//!   `on_event(ptr: i32, len: i32)`;
//! - may import `ferrocast.act(ptr: i32, len: i32)` (a step as JSON, `{"type":"scene","name":"BRB"}`)
//!   and `ferrocast.log(ptr: i32, len: i32)` (a line for the log), nothing else.
//!
//! Budgets: each call may run so many instructions (fuel) and the module may grow to 16 MB;
//! over either, the call stops (a trap) and the plugin is told nothing more after three in a
//! row. At most 10 steps per event.

use serde_json::Value;
use wasmi::{
    Caller, Config, Engine, Extern, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

/// Instructions a call may run (a few milliseconds on a laptop).
pub const FUEL: u64 = 20_000_000;
/// The most memory a plugin may have.
pub const MEMORY: usize = 16 * 1024 * 1024;
/// Steps a plugin may ask for per event, and the most bytes of one step or log line.
pub const MOST_STEPS: usize = 10;
const MOST_BYTES: i32 = 4096;
/// The most an event's JSON can be.
const MOST_EVENT: usize = 8 * 1024;

#[derive(Debug)]
struct Host {
    limits: StoreLimits,
    steps: Vec<Value>,
    logs: Vec<String>,
}

/// A loaded plugin, ready to be told about events.
pub struct Logic {
    store: Store<Host>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    on_event: TypedFunc<(i32, i32), ()>,
}

impl std::fmt::Debug for Logic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logic")
    }
}

/// What a call gave: the steps asked for and the lines logged.
#[derive(Debug, Default, PartialEq)]
pub struct Said {
    pub steps: Vec<Value>,
    pub logs: Vec<String>,
}

/// Reads `len` bytes at `ptr` of the caller's memory, within bounds and the limit.
fn read(caller: &Caller<'_, Host>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    if ptr < 0 || !(0..=MOST_BYTES).contains(&len) {
        return None;
    }
    let mem = match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => return None,
    };
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf).ok()?;
    Some(buf)
}

/// Checks a module and loads it → what's wrong, in words.
pub fn load(wasm: &[u8]) -> Result<Logic, String> {
    if !wasm.starts_with(b"\0asm") {
        return Err("its module isn't WebAssembly".into());
    }
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).map_err(|e| format!("its module doesn't load ({e})"))?;
    for import in module.imports() {
        if import.module() != "ferrocast" || !matches!(import.name(), "act" | "log") {
            return Err(format!(
                "its module asks for \"{}.{}\", which plugins can't have (only ferrocast.act and ferrocast.log)",
                import.module(),
                import.name()
            ));
        }
    }
    let mut store = Store::new(
        &engine,
        Host {
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).instances(1).tables(4).table_elements(10_000).build(),
            steps: Vec::new(),
            logs: Vec::new(),
        },
    );
    store.limiter(|h| &mut h.limits);
    store.set_fuel(FUEL).map_err(|e| e.to_string())?;
    let mut linker = <Linker<Host>>::new(&engine);
    linker
        .func_wrap("ferrocast", "act", |caller: Caller<'_, Host>, ptr: i32, len: i32| {
            let step = read(&caller, ptr, len).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let mut caller = caller;
            let h = caller.data_mut();
            if let Some(s) = step
                && h.steps.len() < MOST_STEPS
            {
                h.steps.push(s);
            }
        })
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap("ferrocast", "log", |caller: Caller<'_, Host>, ptr: i32, len: i32| {
            let line = read(&caller, ptr, len).map(|b| String::from_utf8_lossy(&b).into_owned());
            let mut caller = caller;
            let h = caller.data_mut();
            if let Some(l) = line
                && h.logs.len() < MOST_STEPS
            {
                h.logs.push(l);
            }
        })
        .map_err(|e| e.to_string())?;
    let instance: Instance =
        linker.instantiate_and_start(&mut store, &module).map_err(|e| format!("its module doesn't start ({e})"))?;
    let memory = instance.get_memory(&store, "memory").ok_or("its module has no memory to share")?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, "alloc")
        .map_err(|_| "its module has no alloc(len) -> ptr".to_string())?;
    let on_event = instance
        .get_typed_func::<(i32, i32), ()>(&store, "on_event")
        .map_err(|_| "its module has no on_event(ptr, len)".to_string())?;
    Ok(Logic { store, memory, alloc, on_event })
}

impl Logic {
    /// Tells the plugin about `event` (JSON) → what it asked for, or why the call stopped.
    pub fn tell(&mut self, event: &[u8]) -> Result<Said, String> {
        if event.len() > MOST_EVENT {
            return Err("the event is too big to hand over".into());
        }
        self.store.set_fuel(FUEL).map_err(|e| e.to_string())?;
        let h = self.store.data_mut();
        h.steps.clear();
        h.logs.clear();
        let stopped = |e: wasmi::Error| {
            if self_out_of_fuel(&e) { "it ran too long".to_string() } else { format!("it stopped with an error ({e})") }
        };
        let len = event.len() as i32;
        let ptr = self.alloc.call(&mut self.store, len).map_err(stopped)?;
        if ptr < 0 {
            return Err("it had no room for the event".into());
        }
        self.memory
            .write(&mut self.store, ptr as usize, event)
            .map_err(|_| "it gave room outside its memory".to_string())?;
        self.on_event.call(&mut self.store, (ptr, len)).map_err(stopped)?;
        let h = self.store.data_mut();
        Ok(Said { steps: std::mem::take(&mut h.steps), logs: std::mem::take(&mut h.logs) })
    }
}

fn self_out_of_fuel(e: &wasmi::Error) -> bool {
    e.as_trap_code() == Some(wasmi::TrapCode::OutOfFuel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wasm(text: &str) -> Vec<u8> {
        wat::parse_str(text).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A plugin that asks for "switch to BRB" on every event, and logs the event.
    const SCENE: &str = r#"
        (module
          (import "ferrocast" "act" (func $act (param i32 i32)))
          (import "ferrocast" "log" (func $log (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "{\"type\":\"scene\",\"name\":\"BRB\"}")
          (func (export "alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "on_event") (param $p i32) (param $l i32)
            (call $act (i32.const 0) (i32.const 29))
            (call $log (local.get $p) (local.get $l))))
    "#;

    #[test]
    fn a_plugin_hears_an_event_and_asks_for_a_step() {
        let mut p = load(&wasm(SCENE)).unwrap_or_else(|e| panic!("{e}"));
        let said = p.tell(br#"{"type":"follow","name":"Ana"}"#).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(said.steps, vec![serde_json::json!({ "type": "scene", "name": "BRB" })]);
        assert_eq!(said.logs, vec![r#"{"type":"follow","name":"Ana"}"#.to_string()], "it saw the event");
        // Again: fresh each time.
        assert_eq!(p.tell(b"{}").map(|s| s.steps.len()), Ok(1));
    }

    #[test]
    fn a_plugin_that_never_stops_is_stopped() {
        let mut p = load(&wasm(
            r#"(module (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32) (i32.const 0))
                 (func (export "on_event") (param i32 i32) (loop $l (br $l))))"#,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let t = std::time::Instant::now();
        assert_eq!(p.tell(b"{}"), Err("it ran too long".to_string()));
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "stopped soon: {:?}", t.elapsed());
        // And it can be told again afterwards (with fresh fuel), stopping again.
        assert!(p.tell(b"{}").is_err());
    }

    #[test]
    fn a_plugin_cant_take_more_memory_than_its_share() {
        let mut p = load(&wasm(
            r#"(module (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32) (i32.const 0))
                 (func (export "on_event") (param i32 i32)
                   (if (i32.eq (memory.grow (i32.const 1000)) (i32.const -1)) (then unreachable))))"#,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        // 1000 pages are 64 MB: refused, so the module stops itself.
        assert!(p.tell(b"{}").is_err_and(|e| e.contains("error")));
        // A module that starts bigger than its share doesn't load.
        let big = load(&wasm(
            r#"(module (memory (export "memory") 300) (func (export "alloc") (param i32) (result i32) (i32.const 0)) (func (export "on_event") (param i32 i32)))"#,
        ));
        assert!(big.is_err());
    }

    #[test]
    fn only_our_two_imports_and_a_whole_module_load() {
        let wasi = load(&wasm(
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32) (i32.const 0))
                 (func (export "on_event") (param i32 i32)))"#,
        ));
        assert!(wasi.is_err_and(|e| e.contains("wasi_snapshot_preview1.fd_write")), "no files, no network");
        let other = load(&wasm(r#"(module (import "ferrocast" "secrets" (func)) (memory (export "memory") 1))"#));
        assert!(other.is_err_and(|e| e.contains("ferrocast.secrets")));
        assert!(load(b"not wasm").is_err_and(|e| e.contains("isn't WebAssembly")));
        assert!(load(b"\0asm\x01\0\0\0garbage").is_err_and(|e| e.contains("doesn't load")));
        let no_entry = load(&wasm(r#"(module (memory (export "memory") 1))"#));
        assert!(no_entry.is_err_and(|e| e.contains("alloc")));
        let no_handler = load(&wasm(
            r#"(module (memory (export "memory") 1) (func (export "alloc") (param i32) (result i32) (i32.const 0)))"#,
        ));
        assert!(no_handler.is_err_and(|e| e.contains("on_event")));
    }

    #[test]
    fn bad_pointers_and_floods_are_dropped() {
        let mut p = load(&wasm(
            r#"(module
                 (import "ferrocast" "act" (func $act (param i32 i32)))
                 (memory (export "memory") 1)
                 (data (i32.const 0) "{\"type\":\"save_replay\"}")
                 (func (export "alloc") (param i32) (result i32) (i32.const 2048))
                 (func (export "on_event") (param i32 i32) (local $i i32)
                   (call $act (i32.const -5) (i32.const 10))
                   (call $act (i32.const 65530) (i32.const 100))
                   (call $act (i32.const 0) (i32.const 999999))
                   (call $act (i32.const 0) (i32.const 3))
                   (block $done (loop $l
                     (call $act (i32.const 0) (i32.const 22))
                     (local.set $i (i32.add (local.get $i) (i32.const 1)))
                     (br_if $done (i32.eq (local.get $i) (i32.const 50)))
                     (br $l)))))"#,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let said = p.tell(b"{}").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(said.steps.len(), MOST_STEPS, "out-of-bounds, too long and not-JSON dropped; then 10 at most");
        assert!(said.steps.iter().all(|s| s["type"] == "save_replay"));
        assert!(p.tell(&vec![b' '; MOST_EVENT + 1]).is_err(), "an event too big isn't handed over");
    }

    #[test]
    fn a_plugin_giving_room_outside_its_memory_is_caught() {
        let mut p = load(&wasm(
            r#"(module (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32) (i32.const 65535))
                 (func (export "on_event") (param i32 i32)))"#,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.tell(b"{\"a\":1}"), Err("it gave room outside its memory".to_string()));
    }
}
