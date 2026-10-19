//! 端到端测试：直接跑 `cargo build` 产出的 **filewatch.exe**。
//!
//! `CARGO_BIN_EXE_filewatch` 由 Cargo 在 `cargo test` 时注入，指向本次构建的
//! 可执行文件。所以这里验证的是**真实交付物**，而不是库函数的近似——
//! 参数解析、退出码、stdout 格式、降级链路、实时增量全都走真实进程。
//!
//! 这些用例全部**不要求管理员权限**：降级路径本来就是「普通用户也能用」
//! 那条路，正好用来在无人值守的 CI 上验收。

use core_io::test_dir::TestDir;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_filewatch"))
}

fn run(args: &[&str]) -> Output {
    bin()
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("无法启动 filewatch {:?}：{}", args, e))
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

/// 造一个带子目录与文件的临时目录。
fn fixture(tag: &str) -> TestDir {
    let owned = TestDir::new(tag).unwrap();
    let base = owned.path();
    std::fs::create_dir_all(base.join("docs")).unwrap();
    std::fs::create_dir_all(base.join("media")).unwrap();
    std::fs::write(base.join("docs").join("readme.md"), vec![b'r'; 1500]).unwrap();
    std::fs::write(base.join("docs").join("notes.txt"), vec![b'n'; 300]).unwrap();
    std::fs::write(base.join("media").join("clip.mp4"), vec![b'm'; 90_000]).unwrap();
    std::fs::write(base.join("root.bin"), vec![b'x'; 40]).unwrap();
    owned
}

#[test]
fn version_reports_binary_name() {
    let o = run(&["version"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("filewatch"));
}

#[test]
fn help_lists_every_command() {
    let o = run(&["help"]);
    assert!(o.status.success());
    let s = stdout(&o);
    for cmd in [
        "volumes", "probe", "journal", "scan", "top", "monitor", "selftest",
    ] {
        assert!(s.contains(cmd), "帮助里缺少 {} 命令", cmd);
    }
}

#[test]
fn no_args_prints_help_and_exits_zero() {
    let o = run(&[]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("用法"));
}

#[test]
fn unknown_command_exits_two() {
    let o = run(&["definitely-not-a-command"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("未知命令"));
}

#[test]
fn volumes_reports_at_least_one_volume() {
    let o = run(&["volumes"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("检测到"), "缺少统计行：{}", s);
    // 必须明确告知当前是否具备管理员权限（不允许静默降级）
    assert!(
        s.contains("管理员"),
        "volumes 应当说明提权状态：{}",
        s
    );
}

#[test]
fn scan_degraded_json_on_fixture() {
    let owned = fixture("scan");
    let base = owned.path();
    let root = format!("--root={}", base.to_string_lossy());
    let o = run(&["scan", "C:", &root, "--json", "--top=5"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let s = stdout(&o);

    // 后端必须是 traverse：给了 --root 就不该去碰整卷 $MFT
    assert!(
        s.contains("\"backend\":\"traverse\""),
        "应自动切换为遍历后端，实际输出：{}",
        s
    );
    assert!(s.contains("\"files\":4"), "应扫到 4 个文件：{}", s);
    assert!(s.contains("\"dirs\":3"), "应扫到 3 个目录（根 + docs + media）：{}", s);
    // 1500 + 300 + 90000 + 40
    assert!(s.contains("\"logical\":91840"), "逻辑大小应为 91840：{}", s);
    // Top 列表里最大的是 clip.mp4
    assert!(s.contains("clip.mp4"), "Top-N 缺少最大文件：{}", s);

}

#[test]
fn scan_layout_preview_renders_grid() {
    let owned = fixture("layout");
    let base = owned.path();
    let root = format!("--root={}", base.to_string_lossy());
    let o = run(&["scan", "C:", &root, "--layout=70x20", "--top=0", "--exts=0"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("面积图布局预览"), "{}", s);
    assert!(s.contains("Squarified"), "{}", s);
    // 字符网格必然出现方框角
    assert!(s.contains('+'), "网格未渲染：{}", s);
    assert!(s.contains("同层最大重叠"), "{}", s);

}

#[test]
fn scan_ext_and_dedup_sections() {
    let owned = fixture("ext");
    let base = owned.path();
    let root = format!("--root={}", base.to_string_lossy());
    let o = run(&["scan", "C:", &root, "--top=0", "--exts=10", "--dedup"]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("扩展名 Top"), "{}", s);
    assert!(s.contains("口径差异说明"), "{}", s);

}

#[test]
fn invalid_layout_spec_is_rejected() {
    let o = run(&["scan", "C:", "--layout=nonsense"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--layout"));
}

#[test]
fn invalid_rule_is_rejected() {
    let o = run(&["monitor", "C:", "--rule=bogus=1G", "--duration=0s"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("规则错误"));
}

#[test]
fn monitor_detects_new_file_end_to_end() {
    let owned = fixture("live");
    let base = owned.path();
    let root = base.to_string_lossy().to_string();

    // 4 秒监控；期间由本线程制造变更。
    // 不需要 `mut`：`wait_with_output()` 按值消费 child。
    let child = bin()
        .args([
            "monitor",
            &root,
            "--duration=4s",
            "--interval=100ms",
            "--force-traverse",
            "--tree=0",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("无法启动 monitor");

    // 等它完成全量扫描并挂上目录监视
    std::thread::sleep(std::time::Duration::from_millis(1200));
    std::fs::create_dir_all(base.join("incoming")).unwrap();
    std::fs::write(base.join("incoming").join("payload.dat"), vec![b'p'; 20_000]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(700));
    std::fs::write(base.join("incoming").join("payload.dat"), vec![b'q'; 50_000]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(700));
    owned.remove_file("incoming/payload.dat").unwrap();

    let out = child.wait_with_output().expect("monitor 未能正常结束");
    let s = stdout(&out);

    assert!(
        s.contains("ReadDirectoryChangesW"),
        "降级模式应启用目录监视，实际输出：\n{}",
        s
    );
    assert!(s.contains("监控结束"), "未走到退出路径：\n{}", s);
    // 至少捕获到「新增」与「删除」
    assert!(s.contains("新增"), "未捕获新增事件：\n{}", s);
    assert!(s.contains("删除"), "未捕获删除事件：\n{}", s);
    assert!(s.contains("payload.dat"), "事件里没有目标文件：\n{}", s);

}

#[test]
fn selftest_passes_without_admin() {
    let o = run(&["selftest"]);
    let s = stdout(&o);
    // 自检必须把「失败」和「跳过」分开报，跳过不算失败
    assert!(s.contains("结果：通过"), "自检未输出统计行：\n{}", s);
    assert!(
        s.contains("失败 0 /"),
        "自检存在失败项：\n{}\n{}",
        s,
        stderr(&o)
    );
    assert_eq!(o.status.code(), Some(0), "自检退出码应为 0：\n{}", s);
}
