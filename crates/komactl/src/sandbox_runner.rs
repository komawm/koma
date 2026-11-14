//! `komactl sandbox {run,create-jail,revoke-jail}` — launches restricted Wayland GUI containers
//! (`bwrap` or rootless `podman`) inside an isolated `Headless` Koma workspace.
//!
//! ### Defense-in-Depth Container Isolation Invariants
//! 1. **Never Mounts Host `$XDG_RUNTIME_DIR`**: Only the restricted `0600` `jail-<ws_id>.sock`
//!    (and optionally `koma-agent.sock` when `--with-agent-sock` is explicitly passed, NEVER
//!    `koma-admin.sock` or the host's unrestricted `wayland-X` / D-Bus / PipeWire / SSH sockets)
//!    is mounted inside a private `0700` `tmpfs` runtime directory.
//! 2. **Compositor-Enforced Protocol Filtering**: Because `jail-<ws_id>.sock` tags every client
//!    connection `ClientState { restricted: true, jailed_workspace: Some(ws_id), .. }`, the
//!    container cannot access `wp_security_context_manager_v1`, `zwlr_layer_shell_v1`,
//!    `zwlr/ext_data_control`, `zwp_primary_selection`, `zwp_virtual_keyboard`, or `ext_session_lock`,
//!    and every `xdg_toplevel` it maps stays strictly confined to `ws_id`'s offscreen `LazyFbo`.
//! 3. **RAII Socket Revocation**: Unless `--keep-jail` is specified, `JailCleanupGuard` revokes
//!    and unlinks `jail-<ws_id>.sock` via `sandbox.revoke_wayland_jail` as soon as the container exits.

use std::{
    env,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use koma_proto::WorkspaceId;

use crate::admin_rpc::AdminClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntime {
    Bwrap,
    Podman,
}

impl ContainerRuntime {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "bwrap" | "bubblewrap" => Ok(Self::Bwrap),
            "podman" => Ok(Self::Podman),
            other => Err(format!(
                "unsupported sandbox runtime `{other}` (expected `bwrap` or `podman`)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BindSpec {
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct SandboxRunConfig {
    pub runtime: ContainerRuntime,
    /// Specific `ws_id` (`"ws_02"`) or `None` (`auto`) to allocate a fresh `Headless` workspace.
    pub target_ws: Option<String>,
    pub ws_name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub label: String,
    /// Podman OCI image (required when `runtime == ContainerRuntime::Podman`).
    pub image: Option<String>,
    /// Keep network namespace shared (`--share-net` / `--network=pasta`); isolated (`none`) by default!
    pub share_net: bool,
    /// Pass `/dev/dri` into the container for hardware Mesa/EGL/DMA-BUF acceleration (default `true`).
    pub pass_gpu_dri: bool,
    /// Pass `--device=nvidia.com/gpu=all` CDI flag on rootless Podman for hybrid NVIDIA setups.
    pub pass_nvidia_cdi: bool,
    /// Mint a workspace-scoped `CapabilityToken` and bind `koma-agent.sock` (never `koma-admin.sock`!).
    pub with_agent_sock: bool,
    pub extra_binds: Vec<BindSpec>,
    pub keep_jail: bool,
    pub dry_run: bool,
    pub command: Vec<String>,
}

impl Default for SandboxRunConfig {
    fn default() -> Self {
        Self {
            runtime: ContainerRuntime::Bwrap,
            target_ws: None,
            ws_name: "sandbox".to_owned(),
            width: 1920,
            height: 1080,
            scale: 1.0,
            label: "komactl-sandbox".to_owned(),
            image: None,
            share_net: false,
            pass_gpu_dri: true,
            pass_nvidia_cdi: false,
            with_agent_sock: false,
            extra_binds: Vec::new(),
            keep_jail: false,
            dry_run: false,
            command: Vec::new(),
        }
    }
}

struct ActiveAgentMount {
    host_agent_sock: PathBuf,
    scoped_token: String,
}

/// RAII guard ensuring `sandbox.revoke_wayland_jail` runs even on signal/error exit.
struct JailCleanupGuard {
    admin_socket: PathBuf,
    ws_id: String,
    armed: bool,
}

impl Drop for JailCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut client) = AdminClient::connect_at(&self.admin_socket) {
            let _ = client.revoke_wayland_jail(&self.ws_id);
        }
    }
}

pub fn dispatch_sandbox_subcommand(args: &[String]) -> io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("run") => {
            let cfg = parse_sandbox_run_args(&args[1..])
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
            execute_sandbox_run(&cfg)
        }
        Some("create-jail") => {
            let ws_id = args.get(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: komactl sandbox create-jail <ws_id> [label]",
                )
            })?;
            let label = args.get(2).map(String::as_str).unwrap_or("manual-jail");
            let mut client = AdminClient::connect()?;
            let reply = client.create_wayland_jail(ws_id, label)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ws_id": reply.ws_id,
                    "label": reply.label,
                    "restricted": reply.restricted,
                    "socket_path": reply.socket_path,
                }))
                .unwrap()
            );
            Ok(0)
        }
        Some("revoke-jail") => {
            let ws_id = args.get(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: komactl sandbox revoke-jail <ws_id>",
                )
            })?;
            let mut client = AdminClient::connect()?;
            let revoked = client.revoke_wayland_jail(ws_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ws_id": ws_id,
                    "revoked": revoked,
                }))
                .unwrap()
            );
            Ok(0)
        }
        _ => {
            print_sandbox_help();
            Ok(1)
        }
    }
}

pub fn print_sandbox_help() {
    let _ = print_sandbox_help_to(&mut std::io::stderr());
}

/// Writes the `komactl sandbox` help to `out` (stdout for `--help`, stderr for usage errors).
pub fn print_sandbox_help_to(out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(
        out,
        "Usage:
  komactl sandbox run [OPTIONS] [--] <COMMAND> [ARGS...]
  komactl sandbox create-jail <ws_id> [label]
  komactl sandbox revoke-jail <ws_id>

Options for `sandbox run`:
  --runtime <bwrap|podman>   Container engine (default: bwrap; auto-switches to podman if --image is set)
  --ws <ws_id|auto>          Target Headless workspace ID, or `auto` to create one via ws.create (default: auto)
  --name <name>              Workspace name when creating a new Headless workspace (default: sandbox)
  --geometry <WxH[@scale]>   Headless viewport geometry, e.g. 1920x1080 or 2560x1600@1.6 (default: 1920x1080)
  --label <label>            Audit label attached to the jailed Wayland socket (default: komactl-sandbox)
  --image <oci-image>        OCI image for rootless Podman (e.g. fedora:latest, ghcr.io/...)
  --bind <host:guest>        Read-write directory bind mount into the container
  --ro-bind <host:guest>     Read-only directory bind mount into the container
  --share-net                Enable container network access (bwrap host net / podman pasta); isolated by default
  --no-gpu                   Omit /dev/dri GPU passthrough (force software Mesa/Lavapipe rendering)
  --nvidia                   Add `--device=nvidia.com/gpu=all` CDI device flag on Podman
  --with-agent-sock          Mount koma-agent.sock + inject a single-workspace KOMA_CAPABILITY_TOKEN
  --keep-jail                Keep `jail-<ws_id>.sock` alive after the container process exits
  --dry-run                  Print the constructed `bwrap` or `podman` command without spawning"
    )
}

fn parse_sandbox_run_args(args: &[String]) -> Result<SandboxRunConfig, String> {
    let mut cfg = SandboxRunConfig::default();
    let mut explicit_runtime = false;
    let mut idx = 0;

    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            cfg.command.extend(args[idx + 1..].iter().cloned());
            break;
        }
        if !arg.starts_with('-') {
            cfg.command.extend(args[idx..].iter().cloned());
            break;
        }

        match arg {
            "-h" | "--help" => {
                print_sandbox_help();
                return Err("displayed help".to_owned());
            }
            "--runtime" => {
                idx += 1;
                let val = args.get(idx).ok_or("missing value for --runtime")?;
                cfg.runtime = ContainerRuntime::parse(val)?;
                explicit_runtime = true;
            }
            "--ws" => {
                idx += 1;
                let val = args.get(idx).ok_or("missing value for --ws")?;
                cfg.target_ws = (val != "auto").then(|| val.clone());
            }
            "--name" => {
                idx += 1;
                cfg.ws_name = args.get(idx).ok_or("missing value for --name")?.clone();
            }
            "--geometry" => {
                idx += 1;
                let val = args.get(idx).ok_or("missing value for --geometry")?;
                let (w, h, s) = parse_geometry_spec(val)?;
                cfg.width = w;
                cfg.height = h;
                cfg.scale = s;
            }
            "--label" => {
                idx += 1;
                cfg.label = args.get(idx).ok_or("missing value for --label")?.clone();
            }
            "--image" => {
                idx += 1;
                cfg.image = Some(args.get(idx).ok_or("missing value for --image")?.clone());
                if !explicit_runtime {
                    cfg.runtime = ContainerRuntime::Podman;
                }
            }
            "--bind" => {
                idx += 1;
                let spec = args.get(idx).ok_or("missing value for --bind")?;
                cfg.extra_binds.push(parse_bind_spec(spec, false)?);
            }
            "--ro-bind" => {
                idx += 1;
                let spec = args.get(idx).ok_or("missing value for --ro-bind")?;
                cfg.extra_binds.push(parse_bind_spec(spec, true)?);
            }
            "--share-net" => cfg.share_net = true,
            "--no-gpu" => cfg.pass_gpu_dri = false,
            "--nvidia" => cfg.pass_nvidia_cdi = true,
            "--with-agent-sock" => cfg.with_agent_sock = true,
            "--keep-jail" => cfg.keep_jail = true,
            "--dry-run" => cfg.dry_run = true,
            unknown => return Err(format!("unknown option `{unknown}`")),
        }
        idx += 1;
    }

    if cfg.runtime == ContainerRuntime::Podman && cfg.image.is_none() {
        return Err("`--runtime podman` requires `--image <oci-image>`".to_owned());
    }
    if cfg.runtime == ContainerRuntime::Bwrap && cfg.command.is_empty() {
        return Err(
            "`--runtime bwrap` requires a command to execute (e.g. `-- chromium`)".to_owned(),
        );
    }

    Ok(cfg)
}

fn parse_geometry_spec(spec: &str) -> Result<(u32, u32, f64), String> {
    let (res_part, scale_part) = match spec.split_once('@') {
        Some((r, s)) => (r, Some(s)),
        None => (spec, None),
    };
    let (w_str, h_str) = res_part
        .split_once('x')
        .ok_or_else(|| format!("invalid --geometry `{spec}` (expected WIDTHxHEIGHT[@SCALE])"))?;
    let w: u32 = w_str
        .parse()
        .map_err(|_| format!("invalid width in `{spec}`"))?;
    let h: u32 = h_str
        .parse()
        .map_err(|_| format!("invalid height in `{spec}`"))?;
    let scale: f64 = match scale_part {
        Some(s) => s
            .parse()
            .map_err(|_| format!("invalid scale in `{spec}`"))?,
        None => 1.0,
    };
    Ok((w.max(64), h.max(64), scale.clamp(0.5, 4.0)))
}

fn parse_bind_spec(raw: &str, read_only: bool) -> Result<BindSpec, String> {
    let (host, guest) = raw
        .split_once(':')
        .ok_or_else(|| format!("invalid bind spec `{raw}` (expected HOST_PATH:GUEST_PATH)"))?;
    if host.is_empty() || guest.is_empty() {
        return Err(format!("empty path in bind spec `{raw}`"));
    }
    Ok(BindSpec {
        host_path: PathBuf::from(host),
        guest_path: PathBuf::from(guest),
        read_only,
    })
}

fn execute_sandbox_run(cfg: &SandboxRunConfig) -> io::Result<i32> {
    if cfg.dry_run {
        let placeholder_ws = WorkspaceId::new(
            cfg.target_ws
                .clone()
                .unwrap_or_else(|| "ws_dryrun".to_owned()),
        );
        let placeholder_sock = PathBuf::from(format!(
            "/run/user/{}/koma/dryrun/jail-{}.sock",
            rustix::process::getuid().as_raw(),
            placeholder_ws
        ));
        let placeholder_agent = cfg.with_agent_sock.then(|| ActiveAgentMount {
            host_agent_sock: PathBuf::from(format!(
                "/run/user/{}/koma/dryrun/koma-agent.sock",
                rustix::process::getuid().as_raw()
            )),
            scoped_token: "koma_cap_dryrun_redacted".to_owned(),
        });

        let cmd = build_container_command(
            cfg,
            &placeholder_ws,
            &placeholder_sock,
            placeholder_agent.as_ref(),
        );
        println!("{}", format_command_shell_preview(&cmd));
        return Ok(0);
    }

    let mut admin = AdminClient::connect()?;
    let admin_sock_path = admin.instance().admin_socket.clone();
    let agent_sock_path = admin.instance().agent_socket.clone();

    // 1. Resolve or create the target `Headless` workspace
    let ws_id: WorkspaceId = match &cfg.target_ws {
        Some(existing_id) => {
            if !admin.workspace_exists(existing_id)? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "workspace `{existing_id}` does not exist (omit --ws or use `--ws auto`)"
                    ),
                ));
            }
            WorkspaceId::new(existing_id.clone())
        }
        None => {
            let created =
                admin.create_headless_workspace(&cfg.ws_name, cfg.width, cfg.height, cfg.scale)?;
            eprintln!(
                "[komactl] created Headless workspace `{}` ({}x{}@{})",
                created.ws_id, cfg.width, cfg.height, cfg.scale
            );
            created.ws_id
        }
    };

    // 2. Mint the restricted `0600` per-workspace Wayland jail socket (`jail-<ws_id>.sock`)
    let jail = admin.create_wayland_jail(ws_id.as_str(), &cfg.label)?;
    let mut cleanup_guard = JailCleanupGuard {
        admin_socket: admin_sock_path,
        ws_id: ws_id.as_str().to_owned(),
        armed: !cfg.keep_jail,
    };

    eprintln!(
        "[komactl] bound restricted Wayland jail `{}` -> workspace `{}` (restricted={})",
        jail.socket_path.display(),
        ws_id,
        jail.restricted
    );

    // 3. Optional single-workspace `CapabilityToken` + `koma-agent.sock` mount for in-container Agents
    let agent_mount = if cfg.with_agent_sock {
        let token = admin.mint_workspace_scoped_agent_token(&cfg.label, &ws_id, 28_800)?;
        Some(ActiveAgentMount {
            host_agent_sock: agent_sock_path,
            scoped_token: token,
        })
    } else {
        None
    };

    // Drop our `koma-admin.sock` connection BEFORE spawning the container so zero open admin FDs
    // exist in the parent process table!
    drop(admin);

    // 4. Construct and spawn `bwrap` or rootless `podman`
    let mut child_cmd =
        build_container_command(cfg, &ws_id, &jail.socket_path, agent_mount.as_ref());
    let status: ExitStatus = child_cmd.status()?;

    // 5. Explicit RAII cleanup (`sandbox.revoke_wayland_jail`) runs on `cleanup_guard` drop
    if cfg.keep_jail {
        cleanup_guard.armed = false;
        eprintln!(
            "[komactl] --keep-jail active; restricted socket preserved at {}",
            jail.socket_path.display()
        );
    }

    Ok(status.code().unwrap_or(128))
}

fn build_container_command(
    cfg: &SandboxRunConfig,
    ws_id: &WorkspaceId,
    host_jail_sock: &Path,
    agent_mount: Option<&ActiveAgentMount>,
) -> Command {
    match cfg.runtime {
        ContainerRuntime::Bwrap => build_bwrap_command(cfg, ws_id, host_jail_sock, agent_mount),
        ContainerRuntime::Podman => build_podman_command(cfg, ws_id, host_jail_sock, agent_mount),
    }
}

/// Builds an unprivileged `bwrap` (`Bubblewrap`) command line that isolates PID, IPC, UTS, user,
/// and (by default) network namespaces, mounts a private `0700` `XDG_RUNTIME_DIR`, and binds ONLY
/// the restricted `jail-<ws_id>.sock` as `wayland-0`.
fn build_bwrap_command(
    cfg: &SandboxRunConfig,
    ws_id: &WorkspaceId,
    host_jail_sock: &Path,
    agent_mount: Option<&ActiveAgentMount>,
) -> Command {
    let uid = rustix::process::getuid().as_raw();
    let guest_runtime_dir = format!("/run/user/{uid}");
    let guest_wayland_sock = format!("{guest_runtime_dir}/wayland-0");

    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session",
    ]);

    if !cfg.share_net {
        cmd.arg("--unshare-net");
    }

    // Minimal read-only OS tree compatible with Arch Linux / CachyOS merged-`/usr` & FHS distros
    cmd.args(["--ro-bind", "/usr", "/usr"]);
    for (link_src, link_name) in [
        ("usr/bin", "/bin"),
        ("usr/bin", "/sbin"),
        ("usr/lib", "/lib"),
        ("usr/lib64", "/lib64"),
    ] {
        if Path::new(link_name).is_symlink() {
            cmd.args(["--symlink", link_src, link_name]);
        } else if Path::new(link_name).exists() {
            cmd.args(["--ro-bind-try", link_name, link_name]);
        }
    }

    for ro_etc in [
        "/etc/fonts",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/localtime",
        "/etc/ca-certificates",
        "/etc/ssl",
        "/etc/xdg",
    ] {
        cmd.args(["--ro-bind-try", ro_etc, ro_etc]);
    }
    if cfg.share_net {
        for net_etc in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
            cmd.args(["--ro-bind-try", net_etc, net_etc]);
        }
    }

    cmd.args(["--proc", "/proc", "--dev", "/dev"]);
    if cfg.pass_gpu_dri && Path::new("/dev/dri").exists() {
        cmd.args(["--dev-bind", "/dev/dri", "/dev/dri"]);
    }

    // Ephemeral `/tmp`, isolated `$HOME`, and `0700` `$XDG_RUNTIME_DIR` (NEVER host `/run/user/$UID`!)
    cmd.args([
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/run",
        "--perms",
        "0700",
        "--dir",
        &guest_runtime_dir,
        "--perms",
        "0700",
        "--dir",
        "/tmp/sandbox-home",
    ]);

    // Bind ONLY the restricted `jail-<ws_id>.sock` into the sandbox's `XDG_RUNTIME_DIR`
    cmd.arg("--ro-bind")
        .arg(host_jail_sock)
        .arg(&guest_wayland_sock);

    if let Some(agent) = agent_mount {
        let guest_agent_dir = format!("{guest_runtime_dir}/koma/sandbox");
        let guest_agent_sock = format!("{guest_agent_dir}/koma-agent.sock");
        cmd.args(["--perms", "0700", "--dir", &guest_agent_dir]);
        cmd.arg("--ro-bind")
            .arg(&agent.host_agent_sock)
            .arg(&guest_agent_sock);
    }

    for bind in &cfg.extra_binds {
        cmd.arg(if bind.read_only {
            "--ro-bind"
        } else {
            "--bind"
        })
        .arg(&bind.host_path)
        .arg(&bind.guest_path);
    }

    // Scrub inherited host environment (`--clearenv`) so `DISPLAY`, `KOMA_INSTANCE_SIGNATURE`,
    // `SSH_AUTH_SOCK`, and `DBUS_SESSION_BUS_ADDRESS` never leak into the sandbox!
    let lang = env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_owned());
    cmd.args([
        "--clearenv",
        "--setenv",
        "HOME",
        "/tmp/sandbox-home",
        "--setenv",
        "XDG_RUNTIME_DIR",
        &guest_runtime_dir,
        "--setenv",
        "WAYLAND_DISPLAY",
        "wayland-0",
        "--setenv",
        "XDG_SESSION_TYPE",
        "wayland",
        "--setenv",
        "XDG_CURRENT_DESKTOP",
        "koma",
        "--setenv",
        "GDK_BACKEND",
        "wayland",
        "--setenv",
        "QT_QPA_PLATFORM",
        "wayland",
        "--setenv",
        "MOZ_ENABLE_WAYLAND",
        "1",
        "--setenv",
        "ELECTRON_OZONE_PLATFORM_HINT",
        "wayland",
        "--setenv",
        "PATH",
        "/usr/local/bin:/usr/bin:/bin",
        "--setenv",
        "LANG",
        &lang,
        "--setenv",
        "KOMA_JAILED_WORKSPACE",
        ws_id.as_str(),
    ]);

    if let Some(agent) = agent_mount {
        cmd.args([
            "--setenv",
            "KOMA_INSTANCE_SIGNATURE",
            "sandbox",
            "--setenv",
            "KOMA_CAPABILITY_TOKEN",
            &agent.scoped_token,
        ]);
    }

    cmd.arg("--");
    cmd.args(&cfg.command);
    cmd
}

/// Builds a rootless `podman run` command line preserving host UID (`--userns=keep-id`) and GPU
/// render groups (`--group-add keep-groups`), dropping all Linux capabilities, disabling label
/// relabeling on the live socket, and mounting ONLY `jail-<ws_id>.sock` into a private tmpfs.
fn build_podman_command(
    cfg: &SandboxRunConfig,
    ws_id: &WorkspaceId,
    host_jail_sock: &Path,
    agent_mount: Option<&ActiveAgentMount>,
) -> Command {
    const GUEST_RUNTIME_DIR: &str = "/tmp/koma-runtime";

    let mut cmd = Command::new("podman");
    cmd.args([
        "run",
        "--rm",
        "--init",
        "--userns=keep-id",
        "--group-add=keep-groups",
        "--security-opt=no-new-privileges",
        "--security-opt=label=disable",
        "--cap-drop=ALL",
    ]);

    if cfg.share_net {
        cmd.arg("--network=pasta");
    } else {
        cmd.arg("--network=none");
    }

    if cfg.pass_gpu_dri && Path::new("/dev/dri").exists() {
        cmd.arg("--device=/dev/dri");
    }
    if cfg.pass_nvidia_cdi {
        cmd.arg("--device=nvidia.com/gpu=all");
    }

    cmd.arg(format!(
        "--tmpfs={GUEST_RUNTIME_DIR}:rw,nosuid,nodev,mode=0700"
    ));

    let mut wayland_vol = OsString::from(host_jail_sock.as_os_str());
    wayland_vol.push(format!(":{GUEST_RUNTIME_DIR}/wayland-0:ro"));
    cmd.arg("-v").arg(wayland_vol);

    if let Some(agent) = agent_mount {
        let mut agent_vol = OsString::from(agent.host_agent_sock.as_os_str());
        agent_vol.push(format!(
            ":{GUEST_RUNTIME_DIR}/koma/sandbox/koma-agent.sock:ro"
        ));
        cmd.arg("-v").arg(agent_vol);
        cmd.args([
            "-e",
            "KOMA_INSTANCE_SIGNATURE=sandbox",
            "-e",
            &format!("KOMA_CAPABILITY_TOKEN={}", agent.scoped_token),
        ]);
    }

    for bind in &cfg.extra_binds {
        let mut vol = OsString::from(bind.host_path.as_os_str());
        vol.push(":");
        vol.push(bind.guest_path.as_os_str());
        vol.push(if bind.read_only {
            ":ro,nosuid"
        } else {
            ":rw,nosuid"
        });
        cmd.arg("-v").arg(vol);
    }

    cmd.args([
        "-e",
        &format!("XDG_RUNTIME_DIR={GUEST_RUNTIME_DIR}"),
        "-e",
        "WAYLAND_DISPLAY=wayland-0",
        "-e",
        "XDG_SESSION_TYPE=wayland",
        "-e",
        "XDG_CURRENT_DESKTOP=koma",
        "-e",
        "GDK_BACKEND=wayland",
        "-e",
        "QT_QPA_PLATFORM=wayland",
        "-e",
        "MOZ_ENABLE_WAYLAND=1",
        "-e",
        "ELECTRON_OZONE_PLATFORM_HINT=wayland",
        "-e",
        &format!("KOMA_JAILED_WORKSPACE={ws_id}"),
    ]);

    cmd.arg(cfg.image.as_deref().expect("validated in parser"));
    cmd.args(&cfg.command);
    cmd
}

fn format_command_shell_preview(cmd: &Command) -> String {
    let mut parts = Vec::new();
    parts.push(shell_quote(&cmd.get_program().to_string_lossy()));
    for arg in cmd.get_args() {
        parts.push(shell_quote(&arg.to_string_lossy()));
    }
    parts.join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | ':' | '=' | '@' | ',')
        })
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace("'", r#"'"'"'"#))
    }
}
