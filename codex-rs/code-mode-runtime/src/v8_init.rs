use std::sync::OnceLock;

/// Controls whether V8 may generate executable code at runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum V8JitMode {
    #[default]
    Enabled,
    Disabled,
}

struct V8Initialization {
    _platform: v8::SharedRef<v8::Platform>,
    jit_mode: V8JitMode,
}

static V8_INITIALIZATION: OnceLock<Result<V8Initialization, String>> = OnceLock::new();

/// Initializes the process-wide V8 platform with the requested JIT mode.
///
/// Call this before executing any code-mode cells when JIT must be disabled.
/// V8 cannot change JIT mode after initialization, so a later call requesting
/// a different mode returns an error. Code mode initializes V8 with JIT enabled
/// by default when this function has not been called explicitly.
pub fn initialize_v8(jit_mode: V8JitMode) -> Result<(), String> {
    match V8_INITIALIZATION.get_or_init(|| initialize_v8_with_mode(jit_mode)) {
        Ok(initialization) if initialization.jit_mode == jit_mode => Ok(()),
        Ok(initialization) => Err(format!(
            "V8 was already initialized with JIT {}",
            initialization.jit_mode.description()
        )),
        Err(error_text) => Err(error_text.clone()),
    }
}

pub(crate) fn ensure_v8_initialized() -> Result<(), String> {
    match V8_INITIALIZATION.get_or_init(|| initialize_v8_with_mode(V8JitMode::Enabled)) {
        Ok(_) => Ok(()),
        Err(error_text) => Err(error_text.clone()),
    }
}

fn initialize_v8_with_mode(jit_mode: V8JitMode) -> Result<V8Initialization, String> {
    v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
        .map_err(|error_code| format!("failed to initialize ICU data: {error_code}"))?;
    // The pinned V8 can inline Array.prototype.sort with incompatible element kinds.
    // Disable the affected paths in TurboFan and the Maglev/Turbolev frontend until
    // our V8 artifacts include the upstream fix for mixed-element sorting:
    // https://github.com/v8/v8/commit/e0562d87ad9c17042b581582c99237d798572e67
    v8::V8::set_flags_from_string("--no-maglev --no-turbolev --no-turbo-inline-array-builtins");
    match jit_mode {
        V8JitMode::Enabled => {}
        V8JitMode::Disabled => v8::V8::set_flags_from_string("--jitless"),
    }
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform.clone());
    v8::V8::initialize();
    // V8 15.0 lazily initializes the process-wide PartitionAlloc backing pool
    // without synchronization when the first default ArrayBuffer allocator is
    // created. Finish that initialization inside our OnceLock before runtime
    // threads can create their first isolates concurrently. The backing pool
    // belongs to the isolate group and outlives this temporary allocator.
    drop(v8::new_default_allocator());
    Ok(V8Initialization {
        _platform: platform,
        jit_mode,
    })
}

impl V8JitMode {
    fn description(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_v8_initialized;
    use std::sync::Barrier;

    #[test]
    fn concurrent_first_isolates_initialize_array_buffer_pool_once() {
        const CHILD: &str = "CODEX_TEST_CONCURRENT_FIRST_ISOLATES";
        if std::env::var_os(CHILD).is_none() {
            // Each subprocess starts with a fresh V8 singleton, even when other
            // tests in this binary have already initialized it.
            for _ in 0..5 {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "v8_init::tests::concurrent_first_isolates_initialize_array_buffer_pool_once",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "fresh-process concurrent isolate creation failed: {}\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            return;
        }

        let barrier = Barrier::new(16);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    ensure_v8_initialized().unwrap();
                    barrier.wait();
                    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
                    v8::scope!(let scope, isolate);
                    let context = v8::Context::new(scope, Default::default());
                    let scope = &mut v8::ContextScope::new(scope, context);
                    let buffer = v8::ArrayBuffer::new(scope, 64);
                    assert_eq!(buffer.byte_length(), 64);
                });
            }
        });
    }
}
