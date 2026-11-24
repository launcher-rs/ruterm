//! 主机与连接目标：账号管理、目标解析、ssh_config 导入、局域网发现与远程信息探测。

pub(crate) mod config;
pub(crate) mod discovery;
pub(crate) mod remote;
pub(crate) mod sessions;
pub(crate) mod ssh_config;
