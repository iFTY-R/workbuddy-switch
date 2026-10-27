//! 构建期前置检查：前端产物（`dist/`）是否存在。
//!
//! `api.rs` 用 `#[derive(RustEmbed)] #[folder = "../../dist/"]` 把 webui 嵌进二进制，
//! 而 `dist/` 由前端工具链产出、不入库。缺目录时 RustEmbed 抛的是
//! 「folder ... does not exist」加两个 `E0599: no function ... Assets::get`，
//! 对不熟悉构建链的人几乎无法定位到「先跑 npm run build」。
//!
//! 这里只补一条**可读的构建期 warning**，刻意**不**创建占位目录：空 `dist/` 会被
//! 真实嵌进二进制，把「忘了构建前端」从编译失败变成「服务一个空白页面」的静默错误，
//! 比现在更难查。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !dist.join("index.html").is_file() {
        println!(
            "cargo:warning=前端产物缺失（{}）：请先在仓库根目录执行 `npm ci && npm run build`，\
             再编译 wb-switch-server；否则该 binary 会嵌入空白页面。",
            dist.display()
        );
    }
}
