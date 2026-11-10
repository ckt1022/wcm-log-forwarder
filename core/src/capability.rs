// ── 每個 stage 的 WASI 能力限制（capability policy）────────────────────────────
//
// 舊版 build_runtime 以 `wasmtime_wasi::p2::add_to_linker_sync` 把整套 WASI p2
// （filesystem、sockets…）連結給每一個 stage，因此 parser 即使 import 了
// wasi:filesystem 仍可實例化；當時真正擋住存取的只有「WasiCtxBuilder 沒有
// preopen、socket 預設拒絕」，WIT world 的限制並沒有被 host 強制執行。
//
// 新做法分兩層：
//   1. 載入時檢查：Component::from_file 之後讀 component_type().imports()，
//      逐一比對該 stage 的 allowlist；任何未授權的 import 直接以 `[capability]`
//      錯誤拒絕載入（訊息列出 stage 與每一個違規 import）。
//   2. 連結時最小化：linker 只加入該 stage 被允許的 WASI 介面（逐介面
//      add_to_linker），即使第一層被繞過，未授權介面在實例化時也會是
//      unknown import。
//
// 預設政策（全部 deny-by-default）：
//   - 所有 stage：wasi:io、wasi:clocks、wasi:random、wasi:cli 的 stdio／
//     environment／exit／terminal-*。environment 雖然連結，但 WasiCtx 不注入
//     任何環境變數；stdin 不繼承（空串流），只繼承 stdout／stderr。
//   - parse／filter／format：無 filesystem、無 sockets、無 http。
//   - transport：允許 wasi:http（且 host 端 send_request 只放行 endpoint map
//     中的 host:port），無 raw sockets、無 filesystem。
//
// 明確授權（YAML `capabilities:`，預設 deny）：
//   - filesystem: inert  連結 wasi:filesystem 但不給任何 preopen（零目錄），
//                        僅為了讓 wasip1 adapter 編出的 C／Go／C# plugin 能載入；
//                        plugin 看不到任何檔案。
//   - sockets: inert     連結 wasi:sockets，但所有位址檢查一律拒絕、不開 DNS
//                        （C# runtime 會 import tcp／udp 型別）。
//   - sockets: endpoint  僅 transport 可用：連結 wasi:sockets，只允許 TCP
//                        connect 到 endpoint map 中的 host:port（舊 C transport）。

use std::fmt;

use serde::Deserialize;
use wasmtime::Engine;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime_wasi::WasiView;

/// pipeline 的四個 stage。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Parse,
    Filter,
    Format,
    Transport,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Parse => "parse",
            Stage::Filter => "filter",
            Stage::Format => "format",
            Stage::Transport => "transport",
        }
    }

    pub fn from_label(s: &str) -> Option<Stage> {
        match s {
            "parse" => Some(Stage::Parse),
            "filter" => Some(Stage::Filter),
            "format" => Some(Stage::Format),
            "transport" => Some(Stage::Transport),
            _ => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 檔案系統授權等級。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsGrant {
    /// 不連結、載入時拒絕（預設）
    #[default]
    Deny,
    /// 連結介面但零 preopen：plugin 無任何可開啟的目錄
    Inert,
}

/// socket 授權等級。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocketGrant {
    /// 不連結、載入時拒絕（預設）
    #[default]
    Deny,
    /// 連結介面但所有位址一律拒絕、不開 DNS
    Inert,
    /// 僅 transport：只允許 TCP connect 到 endpoint map 中的 host:port
    Endpoint,
}

/// 單一 stage 的明確授權。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub struct StageGrant {
    #[serde(default)]
    pub filesystem: FsGrant,
    #[serde(default)]
    pub sockets: SocketGrant,
}

/// YAML `capabilities:` 區段；未列出的 stage 一律採預設（全部拒絕）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CapabilityConfig {
    #[serde(default)]
    pub parse: StageGrant,
    #[serde(default)]
    pub filter: StageGrant,
    #[serde(default)]
    pub format: StageGrant,
    #[serde(default)]
    pub transport: StageGrant,
}

impl CapabilityConfig {
    pub fn grant(&self, stage: Stage) -> StageGrant {
        match stage {
            Stage::Parse => self.parse,
            Stage::Filter => self.filter,
            Stage::Format => self.format,
            Stage::Transport => self.transport,
        }
    }

    /// `sockets: endpoint` 只對 transport 有意義（其他 stage 沒有 endpoint map 的語意）。
    pub fn validate(&self) -> Result<(), String> {
        for stage in [Stage::Parse, Stage::Filter, Stage::Format] {
            if self.grant(stage).sockets == SocketGrant::Endpoint {
                return Err(format!(
                    "capabilities.{}.sockets=endpoint 只允許用在 transport；其他 stage 請用 deny 或 inert",
                    stage
                ));
            }
        }
        Ok(())
    }
}

/// import 名稱的能力分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportClass {
    /// 所有 stage 皆可用的基礎介面（io／clocks／random／cli stdio 等）
    Base,
    Filesystem,
    Sockets,
    Http,
    /// 本專案 WIT 套件（local:log-process）的介面
    OwnPackage,
    /// 其他一律視為未知
    Unknown,
}

/// 所有 stage 都允許的基礎 WASI 介面（不含版本號）。
const BASE_INTERFACES: &[&str] = &[
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
    "wasi:clocks/monotonic-clock",
    "wasi:clocks/wall-clock",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
];

/// 去掉 `@0.2.x` 版本後綴。
fn strip_version(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

/// 依 import 名稱分類（例如 "wasi:filesystem/types@0.2.0" → Filesystem）。
pub fn classify(name: &str) -> ImportClass {
    let base = strip_version(name);
    if BASE_INTERFACES.contains(&base) {
        ImportClass::Base
    } else if base.starts_with("wasi:filesystem/") {
        ImportClass::Filesystem
    } else if base.starts_with("wasi:sockets/") {
        ImportClass::Sockets
    } else if base.starts_with("wasi:http/") {
        ImportClass::Http
    } else if base.starts_with("local:log-process/") {
        ImportClass::OwnPackage
    } else {
        ImportClass::Unknown
    }
}

/// 一筆違規 import。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub import: String,
    pub reason: String,
}

/// 載入時能力檢查失敗。
#[derive(Debug, Clone)]
pub struct CapabilityError {
    pub stage: Stage,
    pub plugin: String,
    pub violations: Vec<Violation>,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[capability] 拒絕載入 stage={} plugin={}：{} 個未授權 import",
            self.stage,
            self.plugin,
            self.violations.len()
        )?;
        for v in &self.violations {
            write!(f, "\n[capability]   - {} ({})", v.import, v.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for CapabilityError {}

/// 判斷單一 import 在該 stage／授權下是否允許。
/// `has_functions`：本專案套件的 instance import 是否帶有函式
/// （pipeline-process／transport-types 只有型別；帶函式的就是 host 沒提供的能力）。
pub fn check_import(
    stage: Stage,
    grant: StageGrant,
    name: &str,
    has_functions: bool,
) -> Result<(), String> {
    match classify(name) {
        ImportClass::Base => Ok(()),
        ImportClass::Filesystem => match grant.filesystem {
            FsGrant::Inert => Ok(()),
            FsGrant::Deny => Err(format!(
                "filesystem：{} 預設拒絕；若只是 wasip1 adapter 的連帶 import，可明確設定 capabilities.{}.filesystem: inert（零 preopen）",
                stage, stage
            )),
        },
        ImportClass::Sockets => match grant.sockets {
            SocketGrant::Inert | SocketGrant::Endpoint => Ok(()),
            SocketGrant::Deny => Err(format!(
                "raw sockets：{} 預設拒絕{}",
                stage,
                if stage == Stage::Transport {
                    "（transport 請改用 wasi:http；舊 C transport 可設 capabilities.transport.sockets: endpoint）"
                } else {
                    ""
                }
            )),
        },
        ImportClass::Http => {
            if stage == Stage::Transport {
                Ok(())
            } else {
                Err(format!(
                    "wasi:http：只有 transport 可以使用，{} 不允許",
                    stage
                ))
            }
        }
        ImportClass::OwnPackage => {
            if has_functions {
                Err("本專案套件中帶函式的 import，host 未提供此能力".to_string())
            } else {
                Ok(())
            }
        }
        ImportClass::Unknown => Err("不在任何 stage 的 allowlist 中".to_string()),
    }
}

/// 對一組 (import 名稱, 是否帶函式) 做完整檢查，收集所有違規。
pub fn check_imports<'a, I>(
    stage: Stage,
    grant: StageGrant,
    plugin: &str,
    imports: I,
) -> Result<(), CapabilityError>
where
    I: IntoIterator<Item = (&'a str, bool)>,
{
    let violations: Vec<Violation> = imports
        .into_iter()
        .filter_map(|(name, has_fn)| {
            check_import(stage, grant, name, has_fn)
                .err()
                .map(|reason| Violation {
                    import: name.to_string(),
                    reason,
                })
        })
        .collect();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(CapabilityError {
            stage,
            plugin: plugin.to_string(),
            violations,
        })
    }
}

/// 列出 component 的所有 import：(名稱, 是否帶函式)。
pub fn component_imports(engine: &Engine, component: &Component) -> Vec<(String, bool)> {
    component
        .component_type()
        .imports(engine)
        .map(|(name, item)| {
            let has_fn = match &item {
                ComponentItem::ComponentInstance(inst) => inst.exports(engine).any(|(_, e)| {
                    matches!(
                        e,
                        ComponentItem::ComponentFunc(_) | ComponentItem::CoreFunc(_)
                    )
                }),
                ComponentItem::ComponentFunc(_) | ComponentItem::CoreFunc(_) => true,
                _ => false,
            };
            (name.to_string(), has_fn)
        })
        .collect()
}

/// 載入時檢查：component 的 import 必須全部在該 stage 的 allowlist 內。
pub fn check_component(
    stage: Stage,
    grant: StageGrant,
    plugin: &str,
    engine: &Engine,
    component: &Component,
) -> Result<(), CapabilityError> {
    let imports = component_imports(engine, component);
    check_imports(
        stage,
        grant,
        plugin,
        imports.iter().map(|(n, f)| (n.as_str(), *f)),
    )
}

// ── 逐介面連結 ────────────────────────────────────────────────────────────────

/// wasi:io 的 host 狀態只需要 ResourceTable。
struct HasIo;

impl HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

/// io 以外、sync 與 async 共用的非阻塞基礎介面。
fn add_base_nonblocking<T: WasiView + 'static>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    use wasmtime_wasi::p2::bindings::{cli, clocks, random};
    use wasmtime_wasi::random::{WasiRandom, WasiRandomView};

    clocks::wall_clock::add_to_linker::<T, WasiClocks>(l, <T as WasiClocksView>::clocks)?;
    clocks::monotonic_clock::add_to_linker::<T, WasiClocks>(l, <T as WasiClocksView>::clocks)?;
    random::random::add_to_linker::<T, WasiRandom>(l, <T as WasiRandomView>::random)?;
    random::insecure::add_to_linker::<T, WasiRandom>(l, <T as WasiRandomView>::random)?;
    random::insecure_seed::add_to_linker::<T, WasiRandom>(l, <T as WasiRandomView>::random)?;
    cli::exit::add_to_linker::<T, WasiCli>(l, &Default::default(), <T as WasiCliView>::cli)?;
    cli::environment::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::stdin::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::stdout::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::stderr::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::terminal_input::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::terminal_output::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::terminal_stdin::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::terminal_stdout::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    cli::terminal_stderr::add_to_linker::<T, WasiCli>(l, <T as WasiCliView>::cli)?;
    Ok(())
}

/// filesystem 中與 sync／async 無關的 preopens 介面。
fn add_fs_preopens<T: WasiView + 'static>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
    wasmtime_wasi::p2::bindings::filesystem::preopens::add_to_linker::<T, WasiFilesystem>(
        l,
        <T as WasiFilesystemView>::filesystem,
    )
}

/// sockets 中與 sync／async 無關的介面。
fn add_sockets_nonblocking<T: WasiView + 'static>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    use wasmtime_wasi::p2::bindings::sockets;
    use wasmtime_wasi::sockets::{WasiSockets, WasiSocketsView};
    sockets::tcp_create_socket::add_to_linker::<T, WasiSockets>(
        l,
        <T as WasiSocketsView>::sockets,
    )?;
    sockets::udp_create_socket::add_to_linker::<T, WasiSockets>(
        l,
        <T as WasiSocketsView>::sockets,
    )?;
    sockets::instance_network::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
    sockets::network::add_to_linker::<T, WasiSockets>(
        l,
        &Default::default(),
        <T as WasiSocketsView>::sockets,
    )?;
    sockets::ip_name_lookup::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
    Ok(())
}

/// parse／filter／format（sync 呼叫）用：只連結授權範圍內的 WASI 介面。
pub fn add_wasi_to_linker_sync<T: WasiView + 'static>(
    l: &mut Linker<T>,
    grant: StageGrant,
) -> wasmtime::Result<()> {
    use wasmtime_wasi::p2::bindings::sync;

    wasmtime_wasi::p2::bindings::io::error::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    sync::io::poll::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    sync::io::streams::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    add_base_nonblocking(l)?;

    if grant.filesystem == FsGrant::Inert {
        use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
        add_fs_preopens(l)?;
        sync::filesystem::types::add_to_linker::<T, WasiFilesystem>(
            l,
            <T as WasiFilesystemView>::filesystem,
        )?;
    }
    if grant.sockets != SocketGrant::Deny {
        use wasmtime_wasi::sockets::{WasiSockets, WasiSocketsView};
        add_sockets_nonblocking(l)?;
        sync::sockets::tcp::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
        sync::sockets::udp::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
    }
    Ok(())
}

/// transport（async 呼叫）用：基礎介面 + 授權範圍內的 filesystem／sockets；
/// wasi:http 由呼叫端另外以 add_only_http_to_linker_async 連結。
pub fn add_wasi_to_linker_async<T: WasiView + 'static>(
    l: &mut Linker<T>,
    grant: StageGrant,
) -> wasmtime::Result<()> {
    use wasmtime_wasi::p2::bindings as b;

    b::io::error::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    b::io::poll::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    b::io::streams::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    add_base_nonblocking(l)?;

    if grant.filesystem == FsGrant::Inert {
        use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
        add_fs_preopens(l)?;
        b::filesystem::types::add_to_linker::<T, WasiFilesystem>(
            l,
            <T as WasiFilesystemView>::filesystem,
        )?;
    }
    if grant.sockets != SocketGrant::Deny {
        use wasmtime_wasi::sockets::{WasiSockets, WasiSocketsView};
        add_sockets_nonblocking(l)?;
        b::sockets::tcp::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
        b::sockets::udp::add_to_linker::<T, WasiSockets>(l, <T as WasiSocketsView>::sockets)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENY: StageGrant = StageGrant {
        filesystem: FsGrant::Deny,
        sockets: SocketGrant::Deny,
    };

    #[test]
    fn classify_strips_versions() {
        assert_eq!(classify("wasi:io/streams@0.2.0"), ImportClass::Base);
        assert_eq!(classify("wasi:cli/environment@0.2.6"), ImportClass::Base);
        assert_eq!(
            classify("wasi:filesystem/types@0.2.0"),
            ImportClass::Filesystem
        );
        assert_eq!(
            classify("wasi:filesystem/preopens@0.2.0"),
            ImportClass::Filesystem
        );
        assert_eq!(
            classify("wasi:sockets/tcp-create-socket@0.2.0"),
            ImportClass::Sockets
        );
        assert_eq!(
            classify("wasi:http/outgoing-handler@0.2.0"),
            ImportClass::Http
        );
        assert_eq!(
            classify("local:log-process/pipeline-process@0.3.0"),
            ImportClass::OwnPackage
        );
        assert_eq!(classify("wasi:keyvalue/store@0.2.0"), ImportClass::Unknown);
        assert_eq!(classify("env"), ImportClass::Unknown);
    }

    #[test]
    fn default_policy_rust_parser_imports_pass() {
        // 實際 latency_rust_parse.wasm 的 import 清單
        let imports = [
            ("local:log-process/pipeline-process@0.3.0", false),
            ("wasi:clocks/monotonic-clock@0.2.0", false),
            ("wasi:io/error@0.2.0", false),
            ("wasi:io/streams@0.2.0", false),
            ("wasi:cli/environment@0.2.0", false),
            ("wasi:cli/exit@0.2.0", false),
            ("wasi:cli/stderr@0.2.0", false),
        ];
        for stage in [Stage::Parse, Stage::Filter, Stage::Format, Stage::Transport] {
            assert!(
                check_imports(stage, DENY, "p.wasm", imports).is_ok(),
                "stage={stage}"
            );
        }
    }

    #[test]
    fn default_policy_rejects_filesystem_and_names_every_import() {
        let imports = [
            ("wasi:io/streams@0.2.0", false),
            ("wasi:filesystem/types@0.2.0", false),
            ("wasi:filesystem/preopens@0.2.0", false),
        ];
        let err = check_imports(Stage::Parse, DENY, "parser_c_json.wasm", imports).unwrap_err();
        assert_eq!(err.stage, Stage::Parse);
        assert_eq!(err.violations.len(), 2);
        let msg = err.to_string();
        assert!(msg.starts_with("[capability]"));
        assert!(msg.contains("stage=parse"));
        assert!(msg.contains("wasi:filesystem/types@0.2.0"));
        assert!(msg.contains("wasi:filesystem/preopens@0.2.0"));
    }

    #[test]
    fn inert_filesystem_grant_allows_adapter_imports() {
        let g = StageGrant {
            filesystem: FsGrant::Inert,
            sockets: SocketGrant::Deny,
        };
        let imports = [
            ("wasi:filesystem/types@0.2.0", false),
            ("wasi:filesystem/preopens@0.2.0", false),
        ];
        assert!(check_imports(Stage::Format, g, "f.wasm", imports).is_ok());
        // 但不會連帶放行 sockets
        assert!(
            check_imports(
                Stage::Format,
                g,
                "f.wasm",
                [("wasi:sockets/tcp@0.2.0", false)]
            )
            .is_err()
        );
    }

    #[test]
    fn sockets_denied_everywhere_by_default_including_transport() {
        for stage in [Stage::Parse, Stage::Filter, Stage::Format, Stage::Transport] {
            let r = check_import(stage, DENY, "wasi:sockets/tcp-create-socket@0.2.0", false);
            assert!(r.is_err(), "stage={stage}");
        }
        let ep = StageGrant {
            filesystem: FsGrant::Deny,
            sockets: SocketGrant::Endpoint,
        };
        assert!(check_import(Stage::Transport, ep, "wasi:sockets/tcp@0.2.0", false).is_ok());
    }

    #[test]
    fn http_only_for_transport() {
        assert!(check_import(Stage::Transport, DENY, "wasi:http/types@0.2.0", false).is_ok());
        for stage in [Stage::Parse, Stage::Filter, Stage::Format] {
            assert!(check_import(stage, DENY, "wasi:http/outgoing-handler@0.2.0", false).is_err());
        }
    }

    #[test]
    fn own_package_with_functions_is_rejected() {
        // parser_c_bad.wasm 的 forbidden-capability（probe-host-files、connect-to-host）
        assert!(
            check_import(
                Stage::Parse,
                DENY,
                "local:log-process/forbidden-capability@0.3.0",
                true
            )
            .is_err()
        );
        assert!(
            check_import(
                Stage::Parse,
                DENY,
                "local:log-process/pipeline-process@0.3.0",
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn endpoint_socket_grant_only_valid_for_transport() {
        let mut c = CapabilityConfig::default();
        assert!(c.validate().is_ok());
        c.transport.sockets = SocketGrant::Endpoint;
        assert!(c.validate().is_ok());
        c.parse.sockets = SocketGrant::Endpoint;
        assert!(c.validate().is_err());
    }

    #[test]
    fn yaml_defaults_to_deny() {
        let c: CapabilityConfig = serde_yaml::from_str("format:\n  filesystem: inert\n").unwrap();
        assert_eq!(c.format.filesystem, FsGrant::Inert);
        assert_eq!(c.format.sockets, SocketGrant::Deny);
        assert_eq!(c.parse, StageGrant::default());
        assert_eq!(c.transport.sockets, SocketGrant::Deny);
        assert!(serde_yaml::from_str::<CapabilityConfig>("parse:\n  filesystem: full\n").is_err());
    }

    /// 以 WAT 建立帶有指定 import 的最小 component，確認 linker 與載入檢查實際生效。
    fn component_with_import(engine: &Engine, iface: &str) -> Component {
        let wat = format!(
            r#"(component
                (import "{iface}" (instance))
            )"#
        );
        Component::new(engine, wat).unwrap()
    }

    fn test_engine() -> Engine {
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        Engine::new(&cfg).unwrap()
    }

    #[test]
    fn component_type_imports_are_checked() {
        let engine = test_engine();
        let ok = component_with_import(&engine, "wasi:cli/environment@0.2.0");
        assert!(check_component(Stage::Parse, DENY, "ok.wasm", &engine, &ok).is_ok());

        let bad = component_with_import(&engine, "wasi:sockets/tcp-create-socket@0.2.0");
        let err = check_component(Stage::Filter, DENY, "bad.wasm", &engine, &bad).unwrap_err();
        assert_eq!(
            err.violations[0].import,
            "wasi:sockets/tcp-create-socket@0.2.0"
        );
    }

    #[test]
    fn restricted_linker_builds_for_every_grant() {
        let engine = test_engine();
        for grant in [
            DENY,
            StageGrant {
                filesystem: FsGrant::Inert,
                sockets: SocketGrant::Inert,
            },
        ] {
            let mut l: Linker<crate::plugin::HostState> = Linker::new(&engine);
            add_wasi_to_linker_sync(&mut l, grant).unwrap();
        }
    }
}
