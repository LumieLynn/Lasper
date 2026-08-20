# Configuration Reference

Lasper reads one TOML file from:

```text
~/.config/lasper/lasper.toml
```

The file is parsed once when Lasper starts. It can contain these top-level sections:

```toml
[settings]
[theme]
[bootstrap]
```

Missing sections use their built-in defaults.

## Configuration Rules

- Bootstrap configuration is typed. Arbitrary executable paths and arbitrary command-line `flags` are not supported.
- Bootstrap methods are `debootstrap`, `pacstrap`, `dnf5`, and `artifact`.
- Profile names are scoped below a method. For example, `debootstrap/ubuntu` and `pacstrap/ubuntu` are different profiles.
- `default` is a reserved semantic name for the built-in method form. A `profiles.default` table is a partial preset: it may contain only a policy, only visible form values, or both. It is not shown as a second read-only profile.
- A named profile such as `ubuntu-resolute` is shown separately in the wizard and is not editable there. Named profiles must contain every field required for deployment.
- `~` and `~/...` are expanded for configured artifact paths. Other shell expansions such as `$HOME` are not performed.
- The wizard checks the required provider executable when a source is validated. The required executables are `debootstrap`, `pacstrap`, and `dnf5`; `artifact` does not require an executable.

TOML duplicate keys or duplicate tables are invalid. Lasper does not apply a "last definition wins" rule. If parsing fails, the complete file is ignored, Lasper uses built-in defaults, writes the full error to the log, and shows a warning in the status banner with the file location. Only the current nested bootstrap profile namespace is accepted.

## Settings

```toml
[settings]
elevate = false
cli-mode = false
log-buffer-lines = 5000
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `elevate` | boolean | `false` | Start the isolated elevated daemon, equivalent to `lasper -e`. The TUI remains owned by the invoking user. |
| `cli-mode` | boolean | `false` | Skip DBus and use the CLI communication backend. Equivalent to forcing `-c`. |
| `log-buffer-lines` | integer | `5000` | Maximum number of journal lines retained per container in the detail panel. `0` also means `5000`. |

The command-line flags take precedence over the corresponding configuration values. In particular, `lasper -e` always requests elevation and `lasper -c` always enables CLI mode.

## Bootstrap Selection

`default-method` controls the source selected when the creation wizard opens. If it is absent, the wizard starts with copy/clone selected.

```toml
[bootstrap]
default-method = "debootstrap"
```

Each method has its own optional `default-profile`:

```toml
[bootstrap.methods.debootstrap]
default-profile = "ubuntu-resolute"
```

When `default-profile` is omitted, it is implicitly `"default"`. The implicit default is the built-in editable method form. Its policy can be configured without preselecting any distribution release or packages:

```toml
[bootstrap.methods.debootstrap.profiles.default.policy]
release_signatures = "required"
```

With this configuration the debootstrap suite remains empty in the wizard, while the resulting source retains the required Release-signature policy. The same partial-preset rule applies to pacstrap and DNF5 `default` policies.

Visible form values can also be prefilled:

```toml
[bootstrap]
default-method = "debootstrap"

[bootstrap.methods.debootstrap.profiles.default]
suite = "noble"
mirror = "https://mirror.nju.edu.cn/ubuntu"
packages = ["sudo", "zsh"]

[bootstrap.methods.debootstrap.profiles.default.policy]
transport = "https_only"
release_signatures = "required"
allowed_mirror_hosts = ["mirror.nju.edu.cn"]
```

This opens the wizard on `debootstrap/default`, prefilled with `noble`, the mirror, and the package list. The policy and other typed fields remain attached to the source when the visible wizard fields are edited.

To select a separate read-only profile instead:

```toml
[bootstrap]
default-method = "debootstrap"

[bootstrap.methods.debootstrap]
default-profile = "ubuntu-resolute"

[bootstrap.methods.debootstrap.profiles."ubuntu-resolute"]
suite = "resolute"
mirror = "https://mirror.nju.edu.cn/ubuntu"
packages = ["sudo", "zsh"]
```

## Package Inheritance

Each bootstrap provider has a default package set. Profiles implicitly inherit it with `inherit_default_packages = true`, and their `packages` are appended with stable de-duplication. Set `inherit_default_packages = false` when a distribution release uses different package names or already supplies the required components through another package. Lasper then passes only the explicit `packages` plus packages required by selected wizard features; currently a sudo-enabled user always adds `sudo`. The wizard exposes the same choice as `Include default packages` for each editable bootstrap source.

Disabling inheritance is intentionally allowed and produces a deployment warning rather than being rejected on its own. The resulting profile is responsible for supplying a bootable system manager, D-Bus, the selected guest network stack, and any other facilities expected by its container configuration. Pacstrap and DNF5 still require at least one effective package after wizard-required packages such as `sudo` are included; Lasper rejects an empty final transaction instead of silently restoring pacstrap's implicit `base` package or issuing an invalid DNF5 command.

## Provider Policies

Bootstrap policy is provider-specific. There is deliberately no common source-policy table: debootstrap verifies a Release file, pacstrap inherits selected host pacman state, and DNF5 obtains package-signature behavior from repository configuration. A field is exposed only when Lasper can map it to that provider without changing its meaning.

## Debootstrap

Table location:

```toml
[bootstrap.methods.debootstrap.profiles."ubuntu-noble"]
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `suite` | string | required for named profiles | Distribution suite, such as `noble` or `bookworm`. It may be omitted from the partial `default` preset and entered in the wizard. |
| `architecture` | string | omitted | Target architecture passed through the typed `--arch` option. |
| `mirror` | string | omitted | Optional debootstrap mirror URL. |
| `packages` | array of strings | `[]` | Packages appended to the inherited defaults, or the explicit package set when inheritance is disabled. Lasper also adds `sudo` when the user setup requires it. |
| `inherit_default_packages` | boolean | `true` | Include the debootstrap default package set before `packages`. |
| `exclude_packages` | array of strings | `[]` | Packages excluded from the base installation. |
| `extra_suites` | array of strings | `[]` | Additional archive suites used during bootstrap. |
| `variant` | string | omitted | Optional debootstrap variant. |
| `components` | array of strings | `[]` | Optional repository components. |
| `usr_merge` | `provider_default`, `merged`, `unmerged` | `provider_default` | Leave `/usr` merge behavior to debootstrap or force either layout. |
| `dependency_resolution` | `resolve`, `skip_resolution` | `resolve` | Enable normal dependency resolution or pass `--no-resolve-deps`. |
| `log_extra_dependencies` | boolean | `false` | Record additional dependency information in `debootstrap.log`. |
| `policy` | table | provider defaults | Debootstrap mirror and Release verification policy. |

Debootstrap policy fields:

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `transport` | `provider_default`, `https_only` | `provider_default` | Require the explicit mirror to use HTTPS, or leave transport behavior to debootstrap. |
| `release_signatures` | `provider_default`, `required`, `disabled` | `provider_default` | Control OpenPGP verification of the archive Release file. |
| `allowed_mirror_hosts` | array of strings | `[]` | Exact host allowlist for the explicit mirror. |

`release_signatures = "required"` uses `--force-check-sig`; `disabled` uses `--no-check-sig`. Lasper probes the installed debootstrap help at execution time and falls back to the legacy `--force-check-gpg` or `--no-check-gpg` spelling when required. `https_only` and `allowed_mirror_hosts` require an explicit mirror. Debootstrap does not expose a separate package-signature policy, so Lasper does not model one.

The debootstrap defaults are `systemd-sysv`, `libpam-systemd`, `dbus`, `dbus-user-session`, and `systemd-resolved`. They provide the default systemd-nspawn runtime, login/session integration, D-Bus, and the resolver used by Lasper's Veth/Bridge guest network setup.

Debian Bullseye and Ubuntu Jammy ship `systemd-resolved` as part of `systemd`, while Debian Bookworm and Ubuntu Noble use the separate `systemd-resolved` binary package. A legacy profile can therefore replace the defaults without losing the resolver service:

```toml
[bootstrap.methods.debootstrap.profiles."debian-bullseye"]
suite = "bullseye"
inherit_default_packages = false
packages = ["systemd-sysv", "libpam-systemd", "dbus", "dbus-user-session"]
```

Example:

```toml
[bootstrap.methods.debootstrap.profiles."ubuntu-noble"]
suite = "noble"
architecture = "amd64"
mirror = "https://archive.ubuntu.com/ubuntu"
packages = ["zsh", "git"]
inherit_default_packages = true
exclude_packages = ["nano"]
extra_suites = ["noble-updates"]
variant = "minbase"
components = ["main", "universe"]
usr_merge = "merged"
dependency_resolution = "resolve"
log_extra_dependencies = true

[bootstrap.methods.debootstrap.profiles."ubuntu-noble".policy]
transport = "https_only"
release_signatures = "required"
allowed_mirror_hosts = ["archive.ubuntu.com"]
```

## Pacstrap

Table location:

```toml
[bootstrap.methods.pacstrap.profiles."arch-desktop"]
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `packages` | array of strings | `[]` | Packages appended to the inherited defaults, or the explicit package set when inheritance is disabled. |
| `inherit_default_packages` | boolean | `true` | Include the pacstrap default package set before `packages`. |
| `cache` | `host`, `target` | `host` | Use the host package cache (`-c`) or create/use the target cache. |
| `isolation` | `host`, `unshare` | `host` | Use normal execution or pacstrap's unshare mode (`-N`). |
| `dependency_checks` | `check`, `skip_checks` | `check` | Use normal pacman dependency checks or pass `-D`. |
| `policy` | table | host-integrated defaults | Controls which host pacman trust and repository state pacstrap inherits. |

Pacstrap policy fields:

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `keyring` | `copy_host`, `do_not_copy`, `initialize_empty` | `copy_host` | Copy host keys, skip copying them (`-G`), or initialize an empty keyring (`-K`). |
| `mirrorlist` | `copy_host`, `do_not_copy` | `copy_host` | Copy the host mirrorlist or suppress that copy (`-M`). |
| `pacman_config` | `provider_default`, `copy_host` | `provider_default` | Use pacstrap's normal configuration behavior or copy the host pacman configuration (`-P`). |

These are pacstrap's real host-integration controls, not generic signature or transport promises. Typed alternate `pacman.conf` and repository definitions are intentionally not exposed yet.

The pacstrap default package set is `base`. Arch's `systemd` package supplies both networkd and resolved; Lasper enables their services during Veth/Bridge rootfs finalization rather than adding distribution-specific split packages. Pacstrap itself treats an omitted package list as `base`, so a profile that disables inheritance must still produce at least one effective package through `packages` or a selected wizard feature.

Example:

```toml
[bootstrap.methods.pacstrap.profiles."arch-desktop"]
packages = ["zsh", "networkmanager"]
inherit_default_packages = true
cache = "host"
isolation = "unshare"
dependency_checks = "check"

[bootstrap.methods.pacstrap.profiles."arch-desktop".policy]
keyring = "copy_host"
mirrorlist = "copy_host"
pacman_config = "copy_host"
```

## DNF5

Table location:

```toml
[bootstrap.methods.dnf5.profiles."fedora-43"]
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `releasever` | string | required for named profiles | DNF repository release version. It may be entered in the editable default form. |
| `architecture` | string | omitted | Force the target architecture with `--forcearch`. |
| `packages` | array of strings | `[]` | Packages appended to the inherited defaults, or the explicit package set when inheritance is disabled. |
| `inherit_default_packages` | boolean | `true` | Include the Fedora/RHEL-oriented DNF5 default package set before `packages`. |
| `exclude_packages` | array of strings | `[]` | Package specs excluded from the transaction. |
| `only_repositories` | array of repository selectors | `[]` | Enable only these repositories with `--repo`. Cannot be combined with the enable/disable lists. |
| `enable_repositories` | array of repository selectors | `[]` | Additionally enable matching host repositories. |
| `disable_repositories` | array of repository selectors | `[]` | Disable matching host repositories. |
| `metadata` | `provider_default`, `refresh`, `cache_only` | `provider_default` | Use normal metadata behavior, force refresh, or require cached data. |
| `repository` | `host` | required for named profiles | Explicitly use the host DNF repository configuration while the install root is empty. The built-in editable source currently resolves to `host` automatically. |
| `policy` | table | provider defaults | DNF5 package verification and transaction policy. |

DNF5 policy fields:

| Key | Values | Default | Meaning |
| --- | --- | --- | --- |
| `package_signatures` | `repository_config`, `disabled` | `repository_config` | Honor package GPG settings from the selected repository configuration, or pass `--no-gpgchecks`. |
| `weak_dependencies` | `provider_default`, `enabled`, `disabled` | `provider_default` | Honor DNF5 configuration or set the typed `install_weak_deps` option. |
| `documentation` | `provider_default`, `exclude` | `provider_default` | Honor DNF5 configuration or omit package documentation with `--no-docs`. |
| `best_candidate` | `provider_default`, `required`, `allow_older` | `provider_default` | Honor DNF5 configuration, require best candidates, or allow older satisfiable candidates. |

`repository = "host"` is required until Lasper has a typed repository-definition model. Repository metadata verification is not exposed here: DNF5's `--no-gpgchecks` controls package signatures, so presenting it as a metadata policy would be inaccurate.

The DNF5 defaults are `systemd`, `systemd-pam`, `dbus`, `shadow-utils`, `util-linux`, `dnf5`, `systemd-networkd`, and `systemd-resolved`. These names match current Fedora and RHEL-family repositories, where networkd and resolved are separate systemd subpackages. They are not a universal RPM convention: for example, openSUSE packages these components differently. Disable inheritance and provide the repository's package names when using a non-Fedora/RHEL DNF5 source. The selected repository still determines distribution identity, so `fedora-release-container` remains an explicit profile package.

Example:

```toml
[bootstrap.methods.dnf5.profiles."fedora-43"]
releasever = "43"
architecture = "x86_64"
packages = ["fedora-release-container", "zsh"]
inherit_default_packages = true
exclude_packages = ["kernel-debug*"]
only_repositories = ["fedora", "updates"]
metadata = "refresh"
repository = "host"

[bootstrap.methods.dnf5.profiles."fedora-43".policy]
package_signatures = "disabled"
weak_dependencies = "disabled"
documentation = "exclude"
best_candidate = "required"
```

## Artifact Import

`artifact` represents the wizard's local file source. It does not run a bootstrap executable.

```toml
[bootstrap.methods.artifact.profiles."local-arch"]
path = "~/Downloads/arch-rootfs.tar.zst"
format = "tar"
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `path` | string | required | Local tarball or raw image path. `~` and `~/...` are supported. |
| `format` | `auto`, `tar`, `raw` | `auto` | Selects how the artifact is handled. |

With `auto`, `.raw`, `.img`, and their gzip, xz, zstd, or bzip2-compressed forms are treated as raw images; archive-like extensions are treated as tar archives. Explicit `tar` and `raw` values must match the corresponding file extension when the extension is recognizable. Compression is confirmed from the file content before decoding. Raw artifacts use externally managed storage; tar archives are extracted into the selected Lasper storage backend.

## Theme

The `[theme]` table is optional. Only specified values override the detected light or dark base theme; all other colors retain their defaults.

Each color accepts one of these forms:

```toml
[theme]
accent = "cyan"
warning = "#ff8800"
error = { r = 220, g = 60, b = 60 }
text_dim = 245
```

Supported named colors include `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `dark_gray`, `light_red`, `light_green`, `light_yellow`, `light_blue`, `light_magenta`, `light_cyan`, `white`, `orange`, `reset`, and `default`. Hex strings use `#RRGGBB`; integer values are ANSI 256-color indexes. An unknown color name falls back to white.

The complete list of configurable theme keys is grouped below:

```toml
[theme]
# Text and accents
text_primary = "white"
text_secondary = "white"
text_dim = "white"
accent = "cyan"
highlight = "yellow"
highlight_secondary = "white"

# Semantic and container states
success = "green"
warning = "yellow"
error = "red"
state_running = "green"
state_stopped = "white"
state_transitional = "cyan"

# Borders, tabs, and resize mode
border_focused = "cyan"
border_unfocused = "white"
border_disabled = "white"
border_panel_primary = "white"
border_panel_secondary = "white"
tab_active_focused = "yellow"
tab_active_unfocused = "white"
tab_inactive = "white"
resize_focused = "cyan"
resize_unfocused = "white"

# Badges and status bar
badge_root = "red"
badge_readonly = "yellow"
badge_cli = "cyan"
status_info = "cyan"
status_success = "green"
status_warning = "yellow"
status_error = "red"
key_hint_fg = "yellow"
hint_fg = "white"

# Container list
list_selected_focused = "cyan"
list_selected_unfocused = "white"
list_unselected = "white"
list_icon_alive = "green"
list_icon_dead = "red"
list_addr = "cyan"
list_empty = "white"
list_cursor_focused = "yellow"
list_cursor_unfocused = "white"

# Properties and charts
prop_enabled = "green"
prop_disabled = "red"
prop_unknown = "yellow"
prop_transitional = "cyan"
prop_pid = "cyan"
prop_memory = "magenta"
prop_default = "white"
prop_readonly_yes = "yellow"
prop_readonly_no = "white"
chart_cpu = "cyan"
chart_ram = "magenta"
chart_axis = "white"

# Buttons and dialogs
button_focused_fg = "black"
button_focused_bg = "cyan"
button_unfocused_fg = "white"
button_border_focused = "cyan"
button_border_unfocused = "white"
dialog_border = "cyan"
dialog_border_warn = "yellow"
dialog_text = "white"
dialog_host_border = "red"
dialog_host_text = "white"

# Config, help, wizard, and terminal
config_section = "cyan"
config_key = "yellow"
config_value = "white"
help_key = "yellow"
help_border = "cyan"
help_title = "yellow"
help_close_hint = "white"
wizard_border = "cyan"
wizard_footer = "white"
confirm_hint = "green"
cancel_hint = "red"
editor_error = "red"
terminal_insert_border = "yellow"
```

## Complete Example

The following combines the settings, a debootstrap default prefill, a named profile, and a small theme override:

```toml
[settings]
elevate = true
cli-mode = false
log-buffer-lines = 10000

[theme]
accent = "cyan"
warning = "#ff8800"

[bootstrap]
default-method = "debootstrap"

[bootstrap.methods.debootstrap]
default-profile = "default"

[bootstrap.methods.debootstrap.profiles.default]
suite = "noble"
mirror = "https://mirror.nju.edu.cn/ubuntu"
packages = ["sudo", "zsh"]

[bootstrap.methods.debootstrap.profiles.default.policy]
transport = "https_only"
release_signatures = "required"
allowed_mirror_hosts = ["mirror.nju.edu.cn"]

[bootstrap.methods.debootstrap.profiles."ubuntu-resolute"]
suite = "resolute"
mirror = "https://mirror.nju.edu.cn/ubuntu"
packages = ["sudo", "zsh"]
```
