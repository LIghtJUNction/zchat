use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};
use zchat::{
    config::{Account, ConfigStore, PluginConfig, read_json, validate_id},
    engine::{Access, Engine, Origin, inspect_plugin, request},
};
use zchat_core::{ChatError, Value, arr, obj, parse};

const HELP: &str = r#"zchat — one workspace for your messaging accounts

Usage: zchat [--config PATH] COMMAND

  gui                         Open the native zgpui desktop (the default)
  status                      Inspect runtime and AI access policy
  doctor                      Inspect local backend prerequisites
  catalog                     Show provider capabilities and limitations
  schema                      Show the unified operation contract
  accounts list               List account names, without credentials
  accounts add ID PROVIDER --settings FILE [--label NAME] [--plugin ID]
                              Add an account; FILE may be '-' for stdin
                              Optional --ai-read and --ai-write opt in to AI
  accounts remove ID          Remove local configuration; preserve remote data
  accounts policy ID [--ai-read true|false] [--ai-write true|false]
  plugins list                Show native plugins and trust state
  plugins discover            Locate packaged plugin libraries without loading
  plugins inspect PATH --trust
  plugins add ID PATH --trust  Register an explicitly trusted native library
  plugins remove ID           Unregister a plugin with no configured accounts
  plugin-header               Emit the zplugin C ABI header
  invoke ACCOUNT OPERATION [--params JSON | --params-file FILE]
                              Execute one operation; FILE may be '-'
  rpc                         Read unified JSON requests, one per stdin line
  mcp [--allow-write] [--allow-native]
                              Serve MCP over stdio; account policies also apply
  --version                   Print version

Account configuration: ZCHAT_CONFIG or the platform configuration directory.
Secrets can be environment references: {"access_token":{"env":"MATRIX_TOKEN"}}.
Native calls preserve platform-specific features; query catalog for actual support.
"#;

fn main() {
    if let Err(error) = run() {
        let text=obj([("error",error.to_value())]).try_stringify().unwrap_or_else(|_|"{\"error\":{\"code\":\"output_limit\",\"message\":\"Error response exceeds output limit\",\"retryable\":false}}".into());
        eprintln!("{text}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ChatError> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if flag(&mut args, "--version") || flag(&mut args, "-V") {
        println!("zchat {}", zchat::VERSION);
        return Ok(());
    }
    if flag(&mut args, "--help") || flag(&mut args, "-h") {
        print!("{HELP}");
        return Ok(());
    }
    let config_path = option(&mut args, "--config")?.map(PathBuf::from);
    let command = if args.is_empty() {
        "gui".to_string()
    } else {
        args.remove(0)
    };
    match command.as_str() {
        "gui" => {
            let seconds = option(&mut args, "--smoke-seconds")?
                .map(|s| {
                    s.parse::<u64>().map_err(|_| {
                        ChatError::new(
                            "invalid_argument",
                            "--smoke-seconds needs a positive integer",
                        )
                    })
                })
                .transpose()?;
            no_extra(&args)?;
            #[cfg(feature = "gui")]
            {
                zchat::gui::run(config_path, seconds)
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = (config_path, seconds);
                Err(ChatError::new(
                    "gui_not_built",
                    "Install the desktop build or compile with --features gui",
                ))
            }
        }
        "status" | "catalog" | "schema" | "doctor" => {
            no_extra(&args)?;
            let engine = Engine::new(config_path, Access::default())?;
            output(match command.as_str() {
                "status" => engine.status(),
                "catalog" => engine.catalog(),
                "schema" => zchat::schema::manifest(),
                _ => doctor(&engine),
            });
            Ok(())
        }
        "accounts" => accounts(config_path, args),
        "plugins" => plugins(config_path, args),
        "plugin-header" => {
            no_extra(&args)?;
            let mut header = String::new();
            zchat_plugin_api::ImPluginApi::write_c_header(&mut header)
                .map_err(|e| ChatError::new("header_error", e.to_string()))?;
            print!("{header}");
            Ok(())
        }
        "mcp" => {
            let access = Access {
                allow_write: flag(&mut args, "--allow-write"),
                allow_native: flag(&mut args, "--allow-native"),
            };
            no_extra(&args)?;
            if access.allow_native && !access.allow_write {
                return Err(ChatError::new(
                    "invalid_argument",
                    "--allow-native also requires --allow-write",
                ));
            }
            zchat::mcp::stdio(Engine::new(config_path, access)?)
        }
        "invoke" => {
            let inline = option(&mut args, "--params")?;
            let file = option(&mut args, "--params-file")?;
            if inline.is_some() && file.is_some() {
                return Err(ChatError::new(
                    "invalid_argument",
                    "Choose --params or --params-file",
                ));
            }
            if args.len() != 2 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "invoke requires ACCOUNT OPERATION",
                ));
            }
            let params = if let Some(text) = inline {
                parse(&text)?
            } else if let Some(path) = file {
                read_input(&path)?
            } else {
                obj([] as [(&str, Value); 0])
            };
            let value = Engine::new(config_path, Access::default())?
                .call(request(&args[0], &args[1], params), Origin::Human)?;
            output(value);
            Ok(())
        }
        "rpc" => {
            use std::io::{BufRead, Write};
            no_extra(&args)?;
            let mut engine = Engine::new(config_path, Access::default())?;
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let stdout = io::stdout();
            let mut out = stdout.lock();
            loop {
                let mut line = String::new();
                let read = input
                    .by_ref()
                    .take(1024 * 1024 + 1)
                    .read_line(&mut line)
                    .map_err(zchat::config::io_error)?;
                if read == 0 {
                    break;
                }
                if line.len() > 1024 * 1024 {
                    return Err(ChatError::new("input_limit", "JSON request exceeds 1 MiB"));
                }
                if line.trim().is_empty() {
                    continue;
                }
                let parsed = parse(&line);
                let candidate = parsed
                    .as_ref()
                    .ok()
                    .and_then(|v| v.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let id_result = rpc_id(candidate);
                let id = id_result.as_ref().cloned().unwrap_or(Value::Null);
                let result = id_result
                    .and(parsed)
                    .and_then(|v| engine.call(v, Origin::Human));
                writeln!(out, "{}", rpc_reply(id, result)?).map_err(zchat::config::io_error)?;
                out.flush().map_err(zchat::config::io_error)?;
            }
            Ok(())
        }
        "help" => {
            print!("{HELP}");
            Ok(())
        }
        _ => Err(ChatError::new(
            "unknown_command",
            format!("Unknown command {command}. Run zchat --help."),
        )),
    }
}

fn accounts(path: Option<PathBuf>, mut args: Vec<String>) -> Result<(), ChatError> {
    let action = if args.is_empty() {
        "list".into()
    } else {
        args.remove(0)
    };
    let mut store = ConfigStore::open(path)?;
    match action.as_str() {
        "list" => {
            no_extra(&args)?;
            output(obj([("accounts", store.public_accounts())]));
        }
        "add" => {
            let settings = option(&mut args, "--settings")?.ok_or_else(|| {
                ChatError::new(
                    "invalid_argument",
                    "Provide --settings FILE; use '-' to read private JSON from stdin",
                )
            })?;
            let label = option(&mut args, "--label")?;
            let plugin = option(&mut args, "--plugin")?.unwrap_or_else(|| "builtin".into());
            let ai_read = flag(&mut args, "--ai-read");
            let ai_write = flag(&mut args, "--ai-write");
            if args.len() != 2 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "accounts add requires ID PROVIDER --settings FILE",
                ));
            }
            let account = Account::from_value(&obj([
                ("id", args[0].as_str().into()),
                ("provider", args[1].as_str().into()),
                ("label", label.unwrap_or_else(|| args[0].clone()).into()),
                ("plugin", plugin.into()),
                ("config", read_input(&settings)?),
                ("ai_read", ai_read.into()),
                ("ai_write", ai_write.into()),
            ]))?;
            let public = account.public();
            store.add_account(account)?;
            output(public);
        }
        "remove" => {
            if args.len() != 1 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "accounts remove requires ID",
                ));
            }
            if store.accounts.remove(&args[0]).is_none() {
                return Err(ChatError::new("account_not_found", "Unknown account"));
            }
            store.save()?;
            output(obj([("removed", args[0].as_str().into())]));
        }
        "policy" => {
            let read = option(&mut args, "--ai-read")?
                .map(parse_bool)
                .transpose()?;
            let write = option(&mut args, "--ai-write")?
                .map(parse_bool)
                .transpose()?;
            if args.len() != 1 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "accounts policy requires ID",
                ));
            }
            let account = store
                .accounts
                .get_mut(&args[0])
                .ok_or_else(|| ChatError::new("account_not_found", "Unknown account"))?;
            if let Some(value) = read {
                account.ai_read = value;
            }
            if let Some(value) = write {
                account.ai_write = value;
            }
            let public = account.public();
            store.save()?;
            output(public);
        }
        _ => {
            return Err(ChatError::new(
                "unknown_command",
                "Use accounts list, add, remove or policy",
            ));
        }
    }
    Ok(())
}

fn plugins(path: Option<PathBuf>, mut args: Vec<String>) -> Result<(), ChatError> {
    let action = if args.is_empty() {
        "list".into()
    } else {
        args.remove(0)
    };
    let trusted = flag(&mut args, "--trust");
    match action.as_str() {
        "list" => {
            no_extra(&args)?;
            output(Engine::new(path, Access::default())?.plugins());
        }
        "discover" => {
            no_extra(&args)?;
            output(arr(plugin_candidates().into_iter().map(|p| {
                obj([
                    ("path", p.to_string_lossy().as_ref().into()),
                    ("loaded", false.into()),
                ])
            })));
        }
        "inspect" => {
            if args.len() != 1 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "plugins inspect requires PATH --trust",
                ));
            }
            output(inspect_plugin(Path::new(&args[0]), trusted)?);
        }
        "add" => {
            if args.len() != 2 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "plugins add requires ID PATH --trust",
                ));
            }
            validate_id(&args[0])?;
            if args[0] == "builtin" {
                return Err(ChatError::new(
                    "invalid_plugin",
                    "The builtin identifier is reserved",
                ));
            }
            let canonical = std::fs::canonicalize(&args[1]).map_err(zchat::config::io_error)?;
            let manifest = inspect_plugin(&canonical, trusted)?;
            let mut store = ConfigStore::open(path)?;
            if store.plugins.contains_key(&args[0]) {
                return Err(ChatError::new(
                    "plugin_exists",
                    "Remove the existing registration before replacing it",
                ));
            }
            store.plugins.insert(
                args[0].clone(),
                PluginConfig {
                    id: args[0].clone(),
                    path: canonical,
                    trusted,
                    manifest: Some(manifest.clone()),
                },
            );
            store.save()?;
            output(obj([
                ("id", args[0].as_str().into()),
                ("manifest", manifest),
            ]));
        }
        "remove" => {
            if args.len() != 1 {
                return Err(ChatError::new(
                    "invalid_argument",
                    "plugins remove requires ID",
                ));
            }
            let mut store = ConfigStore::open(path)?;
            if store.accounts.values().any(|a| a.plugin == args[0]) {
                return Err(ChatError::new(
                    "plugin_in_use",
                    "Remove this plugin's account configurations first",
                ));
            }
            if store.plugins.remove(&args[0]).is_none() {
                return Err(ChatError::new("plugin_not_found", "Unknown plugin"));
            }
            store.save()?;
            output(obj([("removed", args[0].as_str().into())]));
        }
        _ => {
            return Err(ChatError::new(
                "unknown_command",
                "Use plugins list, discover, inspect, add or remove",
            ));
        }
    }
    Ok(())
}

fn plugin_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) {
        "zchat_providers.dll"
    } else if cfg!(target_os = "macos") {
        "libzchat_providers.dylib"
    } else {
        "libzchat_providers.so"
    };
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        paths.push(parent.join(name));
        paths.push(parent.join("../lib/zchat").join(name));
    }
    #[cfg(target_os = "linux")]
    paths.push(PathBuf::from("/usr/lib/zchat").join(name));
    paths.retain(|p| p.is_file());
    paths.sort();
    paths.dedup();
    paths
}

fn doctor(engine: &Engine) -> Value {
    let tools = ["curl", "deltachat-rpc-server", "simplex-chat"];
    obj([
        ("runtime", engine.status()),
        (
            "tools",
            arr(tools.into_iter().map(|name| {
                obj([
                    ("name", name.into()),
                    ("installed", executable_exists(name).into()),
                ])
            })),
        ),
        ("native_gui", cfg!(feature = "gui").into()),
        (
            "plugins",
            arr(plugin_candidates()
                .iter()
                .map(|p| Value::from(p.to_string_lossy().as_ref()))),
        ),
        ("providers", engine.catalog()),
    ])
}

fn executable_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|p| {
            p.join(name).is_file() || (cfg!(windows) && p.join(format!("{name}.exe")).is_file())
        })
    })
}
fn read_input(path: &str) -> Result<Value, ChatError> {
    if path != "-" {
        return read_json(Path::new(path));
    }
    let mut text = String::new();
    io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(zchat::config::io_error)?;
    if text.len() > 1024 * 1024 {
        return Err(ChatError::new("input_limit", "Input exceeds 1 MiB"));
    }
    parse(&text)
}
fn flag(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        true
    } else {
        false
    }
}
fn option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, ChatError> {
    if let Some(i) = args.iter().position(|a| a == name) {
        if i + 1 >= args.len() {
            return Err(ChatError::new(
                "invalid_argument",
                format!("{name} requires a value"),
            ));
        }
        args.remove(i);
        Ok(Some(args.remove(i)))
    } else {
        Ok(None)
    }
}
fn no_extra(args: &[String]) -> Result<(), ChatError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(ChatError::new(
            "invalid_argument",
            format!("Unexpected arguments: {}", args.join(" ")),
        ))
    }
}
fn parse_bool(value: String) -> Result<bool, ChatError> {
    match value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ChatError::new("invalid_argument", "Use true or false")),
    }
}
fn output(value: Value) {
    match value.try_stringify() {
        Ok(text) => println!("{text}"),
        Err(_) => {
            eprintln!(
                "{{\"error\":{{\"code\":\"output_limit\",\"message\":\"Response exceeds 1 MiB; request a smaller page\",\"retryable\":false}}}}"
            );
            std::process::exit(1);
        }
    }
}

fn rpc_id(id: Value) -> Result<Value, ChatError> {
    match &id {
        Value::Null => Ok(id),
        Value::String(text) if text.len() <= 256 && !text.chars().any(char::is_control) => Ok(id),
        Value::Number(text) if text.len() <= 128 => Ok(id),
        _ => Err(ChatError::new(
            "invalid_request",
            "RPC id must be null, a bounded number or a string of at most 256 bytes",
        )),
    }
}

fn rpc_reply(id: Value, result: Result<Value, ChatError>) -> Result<String, ChatError> {
    let reply = match result {
        Ok(result) => obj([("id", id.clone()), ("result", result)]),
        Err(error) => obj([("id", id.clone()), ("error", error.to_value())]),
    };
    reply.try_stringify().or_else(|_| {
        obj([
            ("id", id),
            (
                "error",
                ChatError::new(
                    "output_limit",
                    "Response exceeds 1 MiB; request a smaller page",
                )
                .to_value(),
            ),
        ])
        .try_stringify()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpc_rejects_large_ids_and_reports_oversized_results_without_panicking() {
        assert!(rpc_id(Value::from("x".repeat(1024 * 1024 - 100))).is_err());
        assert!(rpc_id(obj([("nested", true.into())])).is_err());
        let response =
            rpc_reply("request-1".into(), Ok(Value::from("x".repeat(1024 * 1024)))).unwrap();
        let response = parse(&response).unwrap();
        assert_eq!(response.get_str("id"), Some("request-1"));
        assert_eq!(
            response.get("error").unwrap().get_str("code"),
            Some("output_limit")
        );
    }
}
