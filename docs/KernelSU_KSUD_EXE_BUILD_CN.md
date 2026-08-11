# KernelSU `ksud.exe` 构建说明

本文档用于记录如何在当前仓库中构建可用于 PC 侧 `boot-patch` 的 `ksud.exe`，以及相关的版本切换、资源准备、验证和常见问题处理方式。

适用场景：

- 在 Windows 上构建 `ksud.exe`
- 将 `ksuinit` 和多个 `*_kernelsu.ko` 嵌入到 `ksud.exe`
- 切换到指定发行版 `tag` 后重复构建

## 1. 基本原理

`ksud` 在非 Android 平台下会走 `cli_non_android` 入口，生成的 `ksud.exe` 主要用于：

- `boot-patch`
- `boot-restore`
- `get-sign`
- `supported-kmis`

关键代码位置：

- [userspace/ksud/src/main.rs](../userspace/ksud/src/main.rs)
- [userspace/ksud/src/cli_non_android.rs](../userspace/ksud/src/cli_non_android.rs)
- [userspace/ksud/src/assets.rs](../userspace/ksud/src/assets.rs)

其中 [userspace/ksud/src/assets.rs](../userspace/ksud/src/assets.rs) 规定了资源目录选择规则：

- Android `x86_64` 使用 `userspace/ksud/bin/x86_64`
- 其他平台，包括 Windows、Linux、macOS，统一使用 `userspace/ksud/bin/aarch64`

因此，**在 Windows 上构建 `ksud.exe` 时，需要把资源文件放到 `userspace/ksud/bin/aarch64`。**

## 2. 需要哪些文件

构建完整可用的 `ksud.exe`，至少需要：

- `ksuinit`
- 一个或多个 `*_kernelsu.ko`

通常该目录里还会同时存在：

- `busybox`
- `bootctl`

### 2.1 `ko` 文件命名规则

`ksud` 是通过扫描文件名后缀 `_kernelsu.ko` 来识别 KMI 的，因此文件名必须符合下面格式，例如：

```txt
android12-5.10_kernelsu.ko
android13-5.10_kernelsu.ko
android13-5.15_kernelsu.ko
android14-5.15_kernelsu.ko
android14-6.1_kernelsu.ko
android15-6.6_kernelsu.ko
android16-6.12_kernelsu.ko
```

如果文件名不符合该规则，`supported-kmis` 无法识别它。

## 3. 文件放置目录

所有要嵌入到 Windows 版 `ksud.exe` 的资源文件都放到：

- [userspace/ksud/bin/aarch64](../userspace/ksud/bin/aarch64)

一个典型目录内容如下：

```txt
userspace/ksud/bin/aarch64/
  bootctl
  busybox
  ksuinit
  android12-5.10_kernelsu.ko
  android13-5.10_kernelsu.ko
  android13-5.15_kernelsu.ko
  android14-5.15_kernelsu.ko
  android14-6.1_kernelsu.ko
  android15-6.6_kernelsu.ko
  android16-6.12_kernelsu.ko
```

## 4. 文件来源

### 4.1 `ksuinit`

来源有两种：

1. 本地编译
2. 使用对应 `tag` 的发布产物

源码位置：

- [userspace/ksuinit](../userspace/ksuinit)

CI 参考：

- [.github/workflows/ksuinit.yml](../.github/workflows/ksuinit.yml)

### 4.2 `kernelsu.ko`

来源也有两种：

1. 使用对应 `tag` 的发布产物
2. 在 Linux / CI 环境中按 KMI 单独编译

CI 参考：

- [.github/workflows/build-lkm.yml](../.github/workflows/build-lkm.yml)
- [.github/workflows/ddk-lkm.yml](../.github/workflows/ddk-lkm.yml)

如果当前目标只是做出完整可用的 `ksud.exe`，通常直接使用对应 `tag` 的发布产物最省事。

## 5. 切换到新的发行版 Tag

推荐先切到目标发行版 `tag`，再基于该 `tag` 新建本地分支后继续构建。这样可以避免不同版本的源码和资源混用。

### 5.1 同步并查看 tag

```powershell
git fetch origin --tags
git tag -l
```

如果只想筛选某个系列：

```powershell
git tag -l "v3.*"
```

### 5.2 查看某个 tag 对应的提交

以 `v3.2.5` 为例：

```powershell
git show -s --format="%H%n%D%n%s" v3.2.5
```

### 5.3 基于 tag 新建本地分支

推荐本地分支命名：

```txt
local/v3.2.5
local/v3.2.6
local/v3.3.0
```

示例：

```powershell
git checkout -b local/v3.2.5 v3.2.5
```

或者：

```powershell
git switch -c local/v3.2.5 v3.2.5
```

如果本地还没有这个 `tag`：

```powershell
git fetch origin tag v3.2.5
git checkout -b local/v3.2.5 v3.2.5
```

### 5.4 如果只有 commit hash

先验证提交是否存在：

```powershell
git rev-parse --verify b0bc817b4e966aa6aa830834eaf6ef765d821d40^{commit}
```

然后创建分支：

```powershell
git checkout -b local/v3.2.5 b0bc817b4e966aa6aa830834eaf6ef765d821d40
```

### 5.5 确认当前版本

```powershell
git status --short --branch
git show -s --format="%H%n%D%n%s" HEAD
```

建议在切换到新的 `tag` 后，重新核对 [userspace/ksud/bin/aarch64](../userspace/ksud/bin/aarch64) 中的资源文件是否与当前版本匹配。

## 6. 编译 `ksuinit`

如果你已经有现成的 `ksuinit`，可以跳过本节。

### 6.1 前置条件

- 已安装 Rust
- 已安装 `aarch64-unknown-linux-musl`
- 已安装 Android NDK

先安装 Rust target：

```powershell
rustup target add aarch64-unknown-linux-musl
```

然后使用 NDK 中的 `clang` 作为 linker：

```powershell
$clang = Get-ChildItem "D:\Android\Sdk\ndk" -Recurse -Filter aarch64-linux-android26-clang.cmd -ErrorAction SilentlyContinue |
  Sort-Object FullName -Descending |
  Select-Object -First 1 -ExpandProperty FullName

$env:CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = $clang
$env:RUSTFLAGS = "-C link-arg=-no-pie"

cargo build --package ksuinit --target aarch64-unknown-linux-musl --release
```

产物一般位于：

- [target/aarch64-unknown-linux-musl/release/ksuinit](../target/aarch64-unknown-linux-musl/release/ksuinit)

复制到资源目录：

```powershell
Copy-Item target\aarch64-unknown-linux-musl\release\ksuinit userspace\ksud\bin\aarch64\ksuinit -Force
```

## 7. 编译 `ksud.exe`

当 `ksuinit` 和 `*_kernelsu.ko` 已经放进 [userspace/ksud/bin/aarch64](../userspace/ksud/bin/aarch64) 后，执行：

```powershell
cargo build --release --manifest-path userspace/ksud/Cargo.toml
```

产物位于：

- [target/release/ksud.exe](../target/release/ksud.exe)

如果之前编译过旧版本，且之后替换了 `ksuinit` 或 `ko` 文件，建议先执行：

```powershell
cargo clean
```

再重新构建。

## 8. 统一验证流程

每次编译完成后，统一按下面步骤验证：

```powershell
.\target\release\ksud.exe supported-kmis
.\target\release\ksud.exe boot-patch --help
```

### 8.1 预期结果

如果嵌入成功，`supported-kmis` 应输出类似：

```txt
android12-5.10
android13-5.10
android13-5.15
android14-5.15
android14-6.1
android15-6.6
android16-6.12
```

如果输出为空，通常说明以下情况之一：

- `ko` 文件没有放在 `userspace/ksud/bin/aarch64`
- `ko` 文件名不符合 `*_kernelsu.ko` 规则
- 当前 `ksud.exe` 仍然是旧产物，还没有重新编译

## 9. 实际操作示例

下面给出一个完整示例，假设目标是：

- 在 Windows 上重编一个带内置资源的 `ksud.exe`
- 目标 KMI 为 `android13-5.10`
- 要 patch 的文件为 `boot.img`

### 9.1 准备资源

确认以下文件已准备好：

- `ksuinit`
- `android13-5.10_kernelsu.ko`

复制到资源目录：

```powershell
Copy-Item target\aarch64-unknown-linux-musl\release\ksuinit userspace\ksud\bin\aarch64\ksuinit -Force
Copy-Item .\android13-5.10_kernelsu.ko userspace\ksud\bin\aarch64\android13-5.10_kernelsu.ko -Force
```

检查目录内容：

```powershell
Get-ChildItem userspace\ksud\bin\aarch64
```

### 9.2 重编 `ksud.exe`

```powershell
cargo clean
cargo build --release --manifest-path userspace/ksud/Cargo.toml
```

### 9.3 验证

```powershell
.\target\release\ksud.exe supported-kmis
```

输出中应包含：

```txt
android13-5.10
```

### 9.4 Patch `boot.img`

如果使用内置资源：

```powershell
.\target\release\ksud.exe boot-patch -b .\boot.img --kmi android13-5.10
```

如果指定输出文件名：

```powershell
.\target\release\ksud.exe boot-patch -b .\boot.img --kmi android13-5.10 --out-name patched-boot.img
```

如果不想依赖内置资源，也可以手动指定：

```powershell
.\target\release\ksud.exe boot-patch `
  -b .\boot.img `
  --kmi android13-5.10 `
  -m .\android13-5.10_kernelsu.ko `
  -i .\ksuinit `
  --out-name patched-boot.img
```

## 10. 最短复现流程

如果你的目标只是最快做出一个可用的 `ksud.exe`，建议按下面顺序：

1. 切到目标 `tag`
2. 准备该 `tag` 对应的 `ksuinit`
3. 准备该 `tag` 对应的 `*_kernelsu.ko`
4. 把文件放到 [userspace/ksud/bin/aarch64](../userspace/ksud/bin/aarch64)
5. 执行：

```powershell
cargo clean
cargo build --release --manifest-path userspace/ksud/Cargo.toml
.\target\release\ksud.exe supported-kmis
```

## 11. 常见问题

### 11.1 `supported-kmis` 是空的

优先检查：

1. `ko` 文件是否在 [userspace/ksud/bin/aarch64](../userspace/ksud/bin/aarch64)
2. 文件名是否符合 `android13-5.10_kernelsu.ko` 这种格式
3. [target/release/ksud.exe](../target/release/ksud.exe) 的时间戳是否晚于资源文件放入时间

### 11.2 `cargo clean` 失败，提示 `os error 5`

这通常是 Windows 文件占用问题，一般表示某个 `.exe` 仍被占用。建议：

1. 关闭当前正在运行的 `ksud.exe`
2. 关闭可能占用 `target` 目录的终端或工具
3. 重新执行：

```powershell
cargo clean
```

### 11.3 资源放进去后，`cargo build` 仍然秒过

这通常说明 Cargo 增量构建没有真正触发资源重嵌入。最稳妥的做法仍然是：

```powershell
cargo clean
cargo build --release --manifest-path userspace/ksud/Cargo.toml
```

### 11.4 `ksuinit` 编译失败，提示找不到 target

先执行：

```powershell
rustup target add aarch64-unknown-linux-musl
```

然后确认 `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER` 指向的是 NDK 中真实存在的 `aarch64-linux-android26-clang.cmd`。

## 12. 备注

当前文档描述的是 `ksud.exe` 的 PC 侧构建和资源嵌入流程，不包含：

- Android 设备侧 `ksud` 的完整运行环境
- `manager` APK 的打包
- `kernelsu.ko` 的完整编译细节

如果后续继续补充，建议新增：

- 如何判断设备应该使用哪个 KMI
- 对应 `tag` 的发布产物下载方式
- `kernelsu.ko` 的 Linux / WSL 构建流程
