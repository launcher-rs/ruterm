# Vendored crates

本目录是上游依赖的本地修补副本，用于解决依赖冲突或修复影响本项目的上游问题。
所有副本保留上游许可证（见各子目录的 LICENSE 文件），
修改处以 `RuTerm 本地补丁` / `本机副本` 注释标明，并尽量最小化改动。

## 各副本说明

### ironrdp-tls
上游 IronRDP 的 TLS 辅助库。本地补丁：用系统根证书 + rustls 默认校验器替换
`danger_accept_invalid_certs(true)`，拒绝未受信自签证书（详见 `src/rustls.rs`）。

### picky
上游 picky（PKCS#12 / X.509）。本地补丁：仅把几个 RustCrypto 依赖跟到已发布版本，
裁掉不需要的 jose / pkcs12 默认特性，以与 russh 已解析的 crypto 版本共存；
其余代码原样（详见根 `Cargo.toml` 的 `[patch.crates-io]` 注释）。

### sspi
上游 sspi（Windows SSPI）。本地补丁：
- macOS / iOS 目标依赖钉的 curve25519-dalek / ed25519-dalek 预发布版跟到已发布版本
  （Windows 构建不编译那段，详见 `Cargo.toml`）。
- CredSSP 的 TLS 通道原先用 `NoCertificateVerification` 接受任意证书（中间人可截获
  NLA 凭据）。改为 rustls 默认校验器 + 系统根证书，并移除会把会话密钥落盘的
  `KeyLogFile`（详见 `src/credssp/sspi_cred_ssp/mod.rs` 与 `src/rustls.rs`）。

### cliprdr-native
上游 IronRDP 的 Windows 原生剪贴板后端。本地副本把上游 0.9 的 Windows 文件剪贴板
实现（CF_HDROP / 文件流）回移到 IronRDP 0.17 钉的 cliprdr 0.7 上（0.7 只有文本），
只保留 Windows 后端，Linux 继续走 stub；并补充拖放上传的进度登记。

### rgpui-macros
上游 rgpui 的过程宏库。本地补丁把 `shadow_2xs()` 至 `shadow_2xl()` 等预设阴影统一置空，
避免阴影的半透明像素在终端 canvas 的浮层上被合成为斜纹/三角形灰块（右键菜单、
命令补全面板都中招）；显式 `shadow(vec![...])` 仍保留原行为。

## 许可证

各子目录的 LICENSE 文件为上游原文，未做改动。
修改的源文件以行内注释标明改动点与原因。