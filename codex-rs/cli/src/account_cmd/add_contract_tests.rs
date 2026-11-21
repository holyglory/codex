//! Capture the real handler's output while injecting only OAuth authorization.

use super::*;
use pretty_assertions::assert_eq;
use std::process::Command;

const BEGIN: &str = "<ACCOUNT_ADD_CONTRACT_BEGIN>\n";
const END: &str = "<ACCOUNT_ADD_CONTRACT_END>";

fn captured_output(stream: &[u8]) -> String {
    String::from_utf8(stream.to_vec())
        .expect("UTF-8 fixture output")
        .split_once(BEGIN)
        .expect("fixture start marker")
        .1
        .split_once(END)
        .expect("fixture end marker")
        .0
        .to_string()
}

#[test]
fn add_handler_preserves_human_and_json_output_contracts() {
    let module = module_path!().split_once("::").expect("crate module").1;
    let fixture = format!("{module}::add_output_fixture_child");
    for scenario in [
        "success-human",
        "success-json",
        "cancelled-human",
        "cancelled-json",
    ] {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                &fixture,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CODEX_TEST_ACCOUNT_ADD_CONTRACT", scenario)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .output()
            .expect("execute isolated add fixture");
        assert!(
            output.status.success(),
            "add fixture failed: {scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut stdout = captured_output(&output.stdout);
        let stderr = captured_output(&output.stderr);
        if scenario == "success-json" {
            let all = String::from_utf8(output.stdout).expect("fixture metadata");
            let id = all
                .lines()
                .find_map(|line| line.strip_prefix("ACCOUNT_ID="))
                .expect("generated account id");
            stdout = stdout.replace(id, "<GENERATED_ACCOUNT_ID>");
        }
        let expected_stdout = match scenario {
            "success-human" => "Added account new.\n",
            "success-json" => concat!(
                "{\n  \"schemaVersion\": 1,\n  \"generation\": 42,\n  \"account\": {\n",
                "    \"id\": \"<GENERATED_ACCOUNT_ID>\",\n    \"alias\": \"new\",\n",
                "    \"authMode\": \"chatgpt\",\n    \"email\": \"new@example.test\",\n",
                "    \"enabled\": true,\n    \"priority\": 1000\n  },\n  \"activeAccount\": \"existing\"\n}\n"
            ),
            "cancelled-human" | "cancelled-json" => "",
            _ => unreachable!("declared fixture scenario"),
        };
        let expected_stderr = match scenario {
            "success-human" | "success-json" => "",
            "cancelled-human" => "Error: account login was cancelled\n",
            "cancelled-json" => {
                "{\"schemaVersion\":1,\"error\":{\"code\":\"loginCancelled\",\"message\":\"account login was cancelled\"}}\n"
            }
            _ => unreachable!("declared fixture scenario"),
        };
        assert_eq!(
            (stdout.as_str(), stderr.as_str()),
            (expected_stdout, expected_stderr),
            "{scenario}"
        );
    }
}

#[tokio::test]
#[ignore = "subprocess fixture for add_handler_preserves_human_and_json_output_contracts"]
async fn add_output_fixture_child() {
    let scenario =
        std::env::var("CODEX_TEST_ACCOUNT_ADD_CONTRACT").expect("parent fixture scenario");
    let json = scenario.ends_with("-json");
    let cancelled = scenario.starts_with("cancelled-");
    let home = TempDir::new().expect("temporary fixture home");
    let config = config(&home).await;
    let store = RegistryStore::new(home.path());
    let mut existing = AccountMetadata::new(
        "existing".parse().expect("alias"),
        AuthMode::Chatgpt,
        chrono::Utc::now(),
    );
    existing.id = "acct_000000000000000000000000000000f0"
        .parse()
        .expect("fixed account id");
    let profile = ProfileAuthStorage::new(
        home.path(),
        existing.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("existing profile");
    profile
        .save(&chatgpt_auth("existing-user", "existing-workspace"))
        .expect("seed credentials");
    let credentials = home
        .path()
        .join("accounts")
        .join(existing.id.as_str())
        .join("auth.json");
    let original_bytes = std::fs::read(&credentials).expect("credential bytes");
    let initial = AccountRegistry {
        generation: 41,
        default_account_id: Some(existing.id.clone()),
        accounts: vec![existing.clone()],
        ..AccountRegistry::default()
    };
    store.create(&initial).expect("seed registry");
    let original_registry_bytes = std::fs::read(store.registry_path()).expect("registry bytes");
    print!("{BEGIN}");
    eprint!("{BEGIN}");
    let result = add_with_authorizer(
        &config,
        &store,
        AddArgs {
            alias: "new".parse().expect("alias"),
            device_auth: false,
        },
        json,
        move |profile| async move {
            if cancelled {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "cancelled fixture",
                ))
            } else {
                profile.save(&chatgpt_auth("new-user", "new-workspace"))
            }
        },
    )
    .await;
    if let Err(error) = &result {
        super::super::super::error::print_error(error, json);
    }
    println!("{END}");
    eprintln!("{END}");
    let registry = store.read().expect("read resulting registry");
    if cancelled {
        assert_eq!(
            result.expect_err("cancelled login").kind(),
            AccountErrorKind::LoginCancelled
        );
        assert_eq!(registry, initial);
        assert_eq!(
            std::fs::read(store.registry_path()).expect("unchanged registry bytes"),
            original_registry_bytes
        );
    } else {
        result.expect("successful login");
        let added = registry
            .accounts
            .iter()
            .find(|account| account.alias.as_str() == "new")
            .expect("added account");
        println!("ACCOUNT_ID={}", added.id);
        assert_eq!(registry.generation, 42);
        assert_eq!(registry.default_account_id, initial.default_account_id);
        assert_eq!(&registry.accounts[0], &existing);
        assert!(
            ProfileAuthStorage::new(
                home.path(),
                added.id.clone(),
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::Direct
            )
            .expect("added profile")
            .load()
            .expect("added credentials")
            .is_some()
        );
    }
    assert_eq!(
        std::fs::read(credentials).expect("preserved credentials"),
        original_bytes
    );
    assert_eq!(pending_journal_count(&home), 0);
    let profiles = std::fs::read_dir(home.path().join("accounts"))
        .expect("profile directories")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count();
    assert_eq!(profiles, if cancelled { 1 } else { 2 });
}
