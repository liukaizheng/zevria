# Zevria

[English](README.md) | 简体中文

Zevria 是一个用 Rust 编写的终端编程智能体。它可以检查代码库、准备待审批的计划、编辑文件、运行命令，并在交互式终端界面中委派彼此独立的工作。它也可以作为无头的 Agent Client Protocol（ACP）智能体，供编辑器和其他客户端使用。

<video src="https://github.com/user-attachments/assets/fab67791-6691-473a-b342-975c9924bc7f" controls width="100%" aria-label="Zevria 在交互式终端中处理编程任务"></video>

> **仍在积极开发中。** API、配置、协议和已保存的会话格式都可能发生不兼容变更。

## 亮点

- **协作编写计划：** `/ensemble-plan <prompt>` 会收集已配置 ACP 智能体（默认设置包括 Claude Code 和 Codex）各自独立提出的方案；待你审阅并确认这些方案后，再综合生成一份计划。参与的智能体必须已安装并完成身份验证。
- **Vim 风格的对话导航：** 使用熟悉的 Vim 风格按键，在 Normal 和 Insert 模式间切换、快速浏览消息、复制消息或工具内容，以及折叠或展开对话条目和轮次。

## 开始使用

<a id="preparation"></a>
### 准备工作

运行 Zevria 前请安装以下必需工具；Zevria 安装程序不会捆绑这些工具：

- **[ripgrep（`rg`）](https://github.com/BurntSushi/ripgrep)**：快速递归搜索文件和源代码中的文本或模式，是 Zevria 查找相关文件和代码的搜索工具；默认遵循忽略规则。
- **[RTK](https://github.com/rtk-ai/rtk)**：负责转发 shell 命令，并筛选或压缩命令输出，让结果更简洁。Zevria 的命令工作流需要它；例如，使用 `rtk rg "pattern" src` 搜索。
- **仅限 Windows — [Git for Windows](https://gitforwindows.org/)**：提供 Git Bash，供 Zevria 在原生 Windows 环境中运行命令。若在 WSL 中运行 Zevria，则不需要 Git Bash。

**终端支持：** Zevria 的状态标记（例如 `○`、`◐`、`✓` 和 `✗`）是带颜色的 Unicode 字符，并非特殊图片图标。任何支持 Unicode 字形和颜色的终端都可以显示它们。为获得完整的 TUI 输入体验，请选择支持增强键盘报告的终端；该功能让 Zevria 能区分 `Ctrl+Enter` 和 `Enter`（否则前者可能被当作普通 `Enter`）：

- **[Ghostty](https://ghostty.org/)** — macOS 和 Linux 上可选的兼容终端；支持增强键盘输入和带颜色的 Unicode 字形。
- **[WezTerm](https://wezterm.org/)** — macOS、Linux 和 Windows 上均可使用，支持同样的键盘输入和字形显示功能。
- **其他 Windows 终端** — 请选用与 Zevria 的增强键盘输入及 Unicode/颜色显示兼容的终端。

这些终端是为了可靠显示和按键处理而推荐的选择；状态标记本身不依赖专有图标协议。

### 1. 安装 Zevria

**Linux x64 GNU / Apple Silicon macOS**（包括通过 Rosetta 运行的 Bash）：

```sh
curl -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/liukaizheng/zevria/main/install.sh | bash
```

**Windows x64**，在 Windows PowerShell 5.1 或 PowerShell 7 中运行（不要在 Git Bash 中运行）：

```powershell
irm https://raw.githubusercontent.com/liukaizheng/zevria/main/install.ps1 | iex
```

这些命令会执行下载的安装程序源代码。如果你的安全策略要求先检查代码，请先下载并检查；其他安装方式、源代码固定、手动下载和未签名版本的限制，参见[安装说明](docs/releases.md#standalone-installers)。这些动态 URL 要等相应文件合入 `main` 后才能使用；如果没有稳定版本，安装会报错，而不会擅自改用未经验证的替代版本。

安装程序会验证发布校验和及可执行文件版本，然后只将 Zevria 安装到解析出的用户主目录下的 `.zevria/bin`，并自动设置**用户级** PATH。无需安装 Rust。Linux 需要 Bash 3.2+、curl、tar，以及 sha256sum 或 shasum；macOS 提供受支持的系统 Bash 和相关工具。目前不提供 Intel macOS、Linux ARM64/musl 或 Windows ARM64 版本。

- 设置绝对路径形式的 `ZEVRIA_INSTALL` 可更改可执行文件的安装根目录（不会更改配置或历史记录位置）。支持空格和 Unicode 字符。
- 可固定二进制版本：在 Bash 安装管道末尾使用 `bash -s -- v0.0.1`，或下载 `install.ps1` 后运行 `./install.ps1 -Version v0.0.1`。示例中的版本号仅为占位符，并不表示最新版本。
- 使用 `--no-path-update` / `-NoPathUpdate` 可不修改 PATH 和配置文件。
- 打开新的 shell，或按安装程序打印的说明刷新环境。管道或子进程中的安装程序无法修改父 shell；Windows 的机器级 PATH 仍可能优先于用户级 PATH。
- 再次运行安装程序会替换选定安装根目录中的内容，包括修复同版本安装或降级。要回退到其他版本，请用该版本重新运行安装程序。其他安装根目录和 provider 文件不会改动。Linux 安装还会记录安装根目录，供 Windows 发现 WSL 中的安装。

请配置一个实现 **OpenAI Responses wire protocol** 的模型端点（仅支持 Chat Completions 并不够）。安装程序不会安装[准备工作](#preparation)中列出的工具、WSL 或 Rust，也不会配置 provider 凭据。另请参阅[命令约定](docs/instructions/command-conventions.md)。

**从源码构建：** 如果已有 Rust/Cargo（支持 Rust 2024）、Git、网络访问和本机编译工具，可在代码检出目录中运行：

```sh
cargo install --path crates/zevria --locked
```

从源码构建时，请确保 Cargo 的 bin 目录位于 PATH 中。随后从你希望处理的项目目录启动 Zevria，不必从本仓库启动：

```sh
cd /path/to/your/project
zevria
```

启动时的工作目录决定工作区、项目指导文件、技能和会话存储位置。从仓库的子目录启动不会自动将 Git 根目录选为工作区。

### Windows：优先使用 WSL，Git Bash 作为后备

在 Windows 上，`zevria.exe` 会优先选择已就绪的 WSL 安装，其中必须有兼容的 **Linux 版 Zevria** 以及当前操作所需的工具。它会将整个应用交给 WSL 运行，而不是只转发单条命令。如果 WSL 未就绪，则静默回退到使用 **Git for Windows Bash** 的原生 Windows 环境，不会输出 WSL 警告或探测时捕获的帮助/错误信息。从 PowerShell 启动没有问题；但 PowerShell 不是智能体运行命令所使用的 shell。

使用 `--runtime native` 可跳过 WSL 探测；使用 `--runtime wsl` 可强制使用 WSL 并查看详细就绪状态错误；使用 `--wsl-distro <name>` 可选择发行版而不更改默认发行版。如果所选发行版不存在或未就绪，自动模式也会静默回退。正常的启动、运行环境/配置/状态诊断，以及原生 Git Bash/RTK 错误仍会显示。

Windows 下的 `--acp` 默认使用原生环境。原生历史记录位于 `.zevria/windows/`；WSL 历史记录和全局配置相互独立。启动器不会安装、升级、复制或迁移任何内容；完成 WSL 交接后，即使 Linux 首次运行配置失败，也**绝不会重试原生运行**。

Windows 和 Linux 安装程序应使用相同的固定二进制版本。新版 Windows 启动器会读取 Linux 安装程序写入的 `.zevria/install-root` 记录，包括自定义安装根目录，且不会加载 shell 配置文件；过期记录必须修复。只有包含这项改动构建的 Windows 二进制才支持该发现功能——安装旧版已发布程序并不会让它自动获得新功能。

请参阅 [Windows 设置与验收清单](docs/windows.md)，了解双端安装、原生 RTK/Git Bash、支持的文件系统边界、ACP 和验证限制。初始验收目标为 Windows x86_64 MSVC 和 WSL 2；不承诺支持其他架构或 WSL 1。

### 2. 配置 provider 和模型角色

首次运行时，Zevria 会创建下表中尚不存在的文件，然后退出，不会打开会话：

| 文件 | 用途 |
| --- | --- |
| `~/.zevria/config.toml` | 模型角色分配，以及会话、技能、ACP、ensemble、命令、主题和日志设置。 |
| `~/.zevria/models.jsonc` | Provider 端点、凭据、模型 ID 和模型能力。 |

生成的文件是带注释的设置模板，**不是可直接工作的默认配置**。你必须至少配置一个 provider/model，并为全部五个模型角色指定配置。

以下是最小配置示例。**请将示例端点、API 密钥、模型 ID、令牌限制和推理能力替换为 provider 实际支持的值。** 以下数字和能力仅用于说明，不是 Zevria 从端点自动检测出来的。

`~/.zevria/models.jsonc`：

```jsonc
{
  "providers": {
    "primary": {
      "base_url": "https://provider.example/v1/responses",
      "api_key": "replace-with-api-key",
      "supports_websockets": false,
      "models": {
        "your-model-id": {
          "context_window_tokens": 128000,
          "retained_user_tokens": 10000,
          "reasoning_levels": ["low", "medium", "high"],
          "reasoning_summary_level": "detailed"
        }
      }
    }
  }
}
```

取消注释并替换 `~/.zevria/config.toml` 中的 `[modes]` 配置块：

```toml
[modes]
plan = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
build = { provider = "primary", model = "your-model-id", reasoning_level = "medium" }
review = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
explore = { provider = "primary", model = "your-model-id", reasoning_level = "low" }
builder = { provider = "primary", model = "your-model-id", reasoning_level = "high" }
```

以上五个角色都必须配置，即使它们使用同一个模型也不例外。显式编排使用 `build` 角色；没有单独的 orchestration 模式，也没有第六个模型角色。

重要配置说明：

- `base_url` 必须是**完整的 Responses 端点**，而不是主机根地址。
- 模型对象的键必须是上游的确切模型 ID，不是本地别名。
- API 密钥以字面值保存在 `models.jsonc` 中；不支持环境变量插值或通过 API 密钥环境变量覆盖。请妥善保护此文件。
- JSONC 支持注释和尾随逗号。Provider 兼容性标志、WebSocket 支持、令牌计数和托管搜索均取决于具体端点。
- `ZEVRIA_CONFIG=/path/to/config.toml` 会选择该 TOML 文件**以及同目录下的 `/path/to/models.jsonc`**，不会更改工作区或技能目录。
- `session.max_model_calls` 已移除。请从现有配置中删除；严格解析会将它视为未知字段并拒绝加载。没有替代的调用次数上限。在任务完成、取消或发生其他错误之前，反复调用工具可能增加运行时间和费用；传输重试与上下文容量仍分别受限。

完整配置约定和网关兼容性设置，请参阅 [Responses 兼容 Provider](docs/responses-compatible.md)。配置完成后，请在项目工作区中再次运行 `zevria`。

<a id="vim-style-transcript-navigation"></a>
## Vim 风格的对话导航

Zevria 使用 Vim 风格的键盘模式浏览对话、复制内容和折叠对话条目。它**不是完整的 Vim 编辑器**：这些按键控制 Zevria 的终端界面，同一个按键会根据你正在浏览、编写还是选择而有不同作用。

### 记住三种模式

- **Normal — 浏览。** Zevria 启动时以及提交提示后通常处于此模式。使用 `j` / `k` 滚动，`gg` 跳到顶部，`G` 跳到底部。按 `i` 开始编写，按 `v` 选择对话内容。
- **Insert — 编写。** 在输入框中输入和编辑提示。没有正在进行的补全时，`Enter` 会换行，`Ctrl+Enter` 会提交草稿。按 `Esc` 返回 Normal 模式。
- **Select — 操作对话内容。** 在 Normal 模式下按 `v` 选择一条消息，或快速按两次 `Esc`。选择从当前可见消息开始，不会滚动对话。处于 Message 范围时，使用 `j` / `k` 在消息间移动；按 `Enter` 进入 Block 范围，在该消息的各个部分间移动。按 `Esc` 每次退回一个范围，最后返回 Normal 模式。

`v` 是 Normal 模式的快捷键：在 Insert 模式下，`v` 和 `z` 都是普通文本。如果按 `v` 没有反应，请确认你处于 Normal 模式且当前有可见的对话内容；它不会自动滚动对话来寻找条目。

### 浏览对话

在 **Normal** 模式下：

- `j` / `k`：向下 / 向上滚动。
- `gg` / `G`：跳到顶部 / 底部。连续按两次 `g` 输入 `gg`。
- `PageDown` / `PageUp`（或 `Ctrl+F` / `Ctrl+B`）：向下 / 向上移动一页。
- `Ctrl+D` / `Ctrl+U`：向下 / 向上移动半页。
- `[` / `]`：跳到上一处 / 下一处轮次起点，包括用户提示和已批准的 Plan 交接。从某轮中间按 `[` 时，先回到当前轮的开头；再按一次才到上一轮。到达首尾后不会循环跳转。

轮次跳转会把目标消息的标题放在**对话内容区域的第一行**；即使最后一轮很短，也会在下方留白以保持对齐。跳转保留折叠状态和草稿，保持 Normal 模式，并暂停自动跟随底部；按 `G` 或 `End` 可恢复跟随。在 Insert 模式下，方括号仍是普通文本，在 Select 模式下不会触发轮次跳转。

在 **Select** 模式下，`j` / `k` 移动选择项，而不是滚动。处于 Message 范围时，它们选择下一条 / 上一条消息；按 `Enter` 进入 Block 范围后，则在该消息的内容块间移动。按一次 `Esc` 返回 Message 范围，再按一次退出 Select 模式。在 Select 模式中，`Ctrl+D` / `Ctrl+U` 会跳到下一条 / 上一条用户消息；与 Normal 模式不同，它们不会按半页滚动。

### 复制消息或工具输出

1. 在 Normal 模式下，将目标内容滚动到可见位置，然后按 `v`。
2. 使用 `j` / `k` 选择消息。按 `y` 将消息内容复制到剪贴板。
3. 若只复制其中一部分，按 `Enter` 进入 Block 范围，再用 `j` / `k` 选择内容块。按 `y` 复制该块的文本或工具参数。
4. 若要复制工具输出，选择其工具块并快速按两次 `y`：`yy`。在 Message 范围中，`yy` 与 `y` 相同，不代表复制工具输出。

### 折叠和展开对话内容

先选择要操作的消息或内容块，然后依次按组合中的两个按键，例如先按 `z`，再按 `a`：

- `za`：切换所选消息（Message 范围）或内容块/条目（Block 范围）的折叠状态。
- `zc`：折叠所选消息或内容块/条目。
- `zo`：展开所选消息或内容块/条目。
- `zm`：折叠每个对话轮次中较早的内容，同时保留该轮次中最新的可折叠条目为展开状态。
- `zM`：执行 `zm`，并进一步折叠较早轮次中最后一个符合条件的条目；最近一轮的最后一个符合条件的条目保持展开。
- `zR`：展开所有已折叠的对话内容。

`za`、`zc` 和 `zo` 针对当前选中项操作，因此请在 Select 模式下使用。`zm`、`zM` 和 `zR` 在 Normal 模式下也可用。折叠只改变显示，不会编辑对话记录，也不会改变 Zevria 发给模型的内容。

### 简短练习

Zevria 回复后，你会回到 Normal 模式。试试以下步骤：

1. 按 `G` 滚动到对话底部，然后按 `v` 选择当前可见消息。
2. 按 `k` 移到上一条消息，或按 `j` 再向前移动。
3. 按 `Enter` 查看该消息的内容块。用 `j` / `k` 移动，再按 `y` 复制内容块；在工具块上按 `yy` 可复制工具输出。
4. 按 `Esc` 返回 Message 范围，然后按 `za` 折叠或展开该消息。再按一次 `Esc` 退出 Select 模式。
5. 按 `i` 开始另一条提示，输入内容后按 `Ctrl+Enter` 提交。响应生成期间，Zevria 会回到 Normal 模式。

按 `y` 复制内容；`Ctrl+C` 不是复制命令，可能会取消正在进行的工作。其他常用按键见下方的[终端 UI 控件](#using-the-terminal-ui)章节。

<a id="collaborative-plan-writing"></a>
## 协作编写计划

如果希望多个智能体一起探索任务，并在实现前协助形成一份明确的计划，请使用 `/ensemble-plan`。Zevria 会启动相互独立的 ACP 计划工作智能体并收集各自方案。所有参与者都明确确认其当前方案后，Zevria 才会发布最终计划。存在多个工作智能体时，Zevria 会核实证据并综合方案。这不同于 `/orchestrate`：后者会为 Build 请求委派实现子任务。Ensemble 工作智能体负责提出和讨论计划，不会实现计划。

### 1. 准备智能体

Zevria 自身必须配置好工作用的 provider/model，以完成根计划的综合步骤。工作智能体也必须已配置并可启动。新的配置包含 Codex、Claude Code 和 Zevria 作为计划工作智能体。默认的 Codex 和 Claude Code 条目通过 `npx` 启动 ACP 适配器；请确保 `npx` 可用，并按各智能体的常规 CLI 流程完成身份验证。Zevria 无法替你登录外部智能体。

如果**只想使用 Codex 和 Claude Code** 作为工作智能体，请在 `~/.zevria/config.toml` 的 `[ensemble]` 部分设置 `plan_agents`。如果该表已存在，请替换其 `plan_agents` 值，不要再添加第二个 `[ensemble]` 表：

```toml
[ensemble]
plan_agents = ["codex", "claude"]
```

这里的名称是配置 ID（`codex` 和 `claude`）；界面上显示的工作智能体名称分别是 **Codex** 和 **Claude Code**。此设置只选择工作智能体，不会更改 Zevria 的根综合模型；后者仍使用已配置的 Plan 角色。自定义智能体和设置详情请参阅 [ensemble 配置指南](docs/ensemble.md#configuration-and-resource-lifetime)。

### 2. 启动计划流程

在根输入框中按 `i`，输入 `/ensemble-plan` 和清晰的任务描述。请说明目标、约束、重要的现有行为，以及希望最终计划涵盖的内容。例如：

```text
/ensemble-plan 为大型文件设计一个可续传上传功能。
比较简单实现与分块实现。保留现有的取消行为，指出可能涉及的文件和测试，
并说明哪些假设需要我做决定。请返回分步实施计划；不要修改代码。
```

按 `Ctrl+Enter` 提交。Zevria 会启动已配置的工作智能体，其进度和方案会显示在 ensemble 对话中。

### 3. 审阅方案并要求修改

在根对话中按 `v` 选择 ensemble 消息，然后按 `Enter` 进入 Block 范围。使用 `j` / `k` 移动到某个工作智能体的行，再按 `Enter` 打开它的面板。阅读其方案；如有帮助，可与另一位工作智能体的面板比较。按 `Ctrl+O` 返回根 ensemble。每个工作智能体都有自己的对话和方案。

工作智能体完成任务，**不等于**确认其方案。若要要求修改，请在该工作智能体的面板中按 `i`，输入普通反馈（例如“补充迁移路径，并为上传中断编写测试”），然后按 `Ctrl+Enter`。反馈被接受后，该工作智能体之前的确认会撤销。等待它发布完整的新 Markdown 计划，审阅新版本并确认。

方案准备就绪后，在该工作智能体的面板中按 `i`，输入 `/confirm`，再按 `Ctrl+Enter`。这会确认当前显示的确切修订版本，将其纳入 ensemble 的最终计划；这**不会授权修改代码**。每个参与的工作智能体都必须确认当前方案。Zevria 不会擅自将未确认或失败的工作智能体视为已批准。

### 4. 获取综合计划，并决定是否实施

所有仍参与的工作智能体都明确确认当前方案后，Zevria 才会完成计划流程。如果有多个工作智能体，Zevria 会检查相关证据并综合方案；如果仍有重要偏好未解决，它可能会向你提问。如果启动流程时恰好只有一个工作智能体，Zevria 会原样发布该工作智能体确认过的 Markdown，而不会再让第二个模型改写。

已发布的计划只是规划结果，**不会自动开始实施**。准备好修改后，请明确选择 `/implement`，在当前会话中继续；或选择 `/implement-fresh`，在新会话中开始实施。其他工作智能体控制、故障恢复和边缘情况，请参阅完整的 [ensemble 指南](docs/ensemble.md)。

<a id="using-the-terminal-ui"></a>
## 使用终端 UI

上文的 [Vim 风格对话导航](#vim-style-transcript-navigation)介绍了模式、提示提交、移动、复制和折叠；[协作编写计划](#collaborative-plan-writing)介绍了 `/ensemble-plan`。本节介绍其他终端 UI 控件和弹窗。

在 Insert 模式中输入 `/` 可浏览内置命令补全。按 `Enter` 接受高亮行；仅当整个草稿是有效且不需要参数的内置命令时，才会立即执行。需要提示文本的命令、技能和文件引用只会补全文本；若草稿末尾还有其他内容或附加了图片，也不会自动执行。没有匹配结果时按 `Enter` 不会产生操作。

根输入框和仍在运行的计划工作智能体输入框，在工作待处理或运行期间仍可编辑；是否允许提交另一个工作项则单独控制。确认消息不会清除更新的草稿。历史面板和冻结面板为只读。

| 按键 | 操作 |
| --- | --- |
| `Shift+Tab` | 在 Build 和 Plan 之间切换。 |
| `Ctrl+V` | 在输入模式下粘贴系统剪贴板中的图片；若没有图片，则粘贴文本。 |
| `Home` / `End` | 将焦点所在编辑器、列表、对话或对话框中的光标移到开头 / 末尾。 |
| `Tab` / `Ctrl+I` | 可导航面板时打开最新子面板；补全激活时仅接受高亮文本。 |
| `r` | 在 Normal 模式且输入框为空时，恢复暂存的、被拒绝的草稿。 |
| `Ctrl+C` | 仅作用于当前焦点区域：输入框会先清空草稿；对话区会取消符合条件的本地工作。只有空闲的根会话可以退出。对话框仅在本地关闭或取消。 |

Plan 审阅默认选中 **Revise（修改）**。使用方向键、数字键或 `n` 进行选择；按 `Enter` 执行当前符合条件的选项。按 `Esc` 和 `Ctrl+C` 只会隐藏审阅界面，不会批准或修改计划；按 `p` 可重新打开。隐藏的 Ready Plan 仍要求你明确决定后，才能提交新工作。取消和所有权详情请参阅 [TUI 交互约定](docs/tui-interaction.md)。

剪贴板图片输入需要桌面剪贴板访问权限，并且终端要能转发按键。限制以及无头/SSH 环境说明见[图片输入](docs/image-input.md)。

### 引用工作区文件

在 Insert 模式中，可在草稿开头或空白字符之后输入 `@`，然后按文件名或相对路径搜索。例如，`Explain @app` 可以补全为 `Explain @crates/tui/src/app.rs `。引用也适用于多行提示、同一提示中多次引用以及技能/ensemble 参数中。电子邮件地址和转义后的 `\@` 仍按普通文本处理。

**Files** 弹窗会显示完整路径。使用 Up/Down、PageUp/PageDown 和 Home/End 选择；按 Enter、Tab 或 Ctrl+I 插入，但不会提交。加载中或空结果行不能被接受。按 Esc 会关闭弹窗但保留查询；更改查询或光标位置可以重新打开。Ctrl+Enter 始终提交实际草稿，即使存在尚未解析的引用也一样，不会接受当前高亮结果。

选择结果**只是插入文本，不会附加文件内容**。含空格、引号或反斜杠的路径使用可逆的引号形式，例如 `@"docs/my file.md"`；在引号内，`\"` 表示引号，`\\` 表示反斜杠。已有图片和周围草稿文本都会保留；一次撤销即可还原查询文本。

搜索使用启动时捕获的工作区，而不是 Git 根目录。它遵循 `.gitignore`、`.ignore` 和适用的父目录/Git 排除规则，即使当前目录不在 Git 仓库中也一样。未被忽略的隐藏文件、配置文件和二进制文件也会纳入索引；`.git` 元数据、目录、根目录外或失效的符号链接，以及不安全的名称都不可选。不会遍历指向目录的符号链接。索引会在后台延迟构建，并在弹窗再次打开时刷新；刷新期间可能仍显示缓存的建议。达到限制或遇到不可读/省略条目时，会显示 **Partial index（索引不完整）** 状态，而不会暗示索引完整。系统不监视文件系统变化；引用也不是文件快照，更不保证路径之后仍然存在。限制和生命周期详情请参阅[文件引用约定](docs/tui-interaction.md#workspace-file-references)。

### 选择工作流

| 命令 | 工作流 |
| --- | --- |
| `/build` | 在当前工作区实施，可选择只读 Explore 子任务。 |
| `/plan` | 调查并准备一份待审批的计划，不实施更改。 |

普通 Build 可以使用 Explore，但不能启动 Builder。`/orchestrate <prompt>` 必须在同一个 `launch_subtasks` 批次中至少启动两个不同且已接受的子任务；它不是持续生效的模式。父任务也可以自行调查并直接实施，然后整合和验证结果。调度器必须配置 `session.max_concurrent_subtasks >= 2`。分别启动单个子任务不符合要求；如果始终无法满足委派条件，最多经过一次纠正性续接后就会失败。下一条提示会恢复为普通 Build。

单独输入 `/orchestrate` 会保留草稿并要求你补充提示。`/build` 和 `/orchestrate <prompt>` 都不会批准或修改待处理的 Plan；请明确处理该计划。包含已移除 Orchestrate 模式的旧对话记录会被拒绝，且不会被改写；请改为启动新的 Build 会话。

其他常用命令：

| 命令 | 用途 |
| --- | --- |
| `/new` | 在此工作区启动空白 Build 对话，保留两种模式各自的模型及推理级别。 |
| `/resume` | 选择此工作区中的先前会话。 |
| `/compact` | 将当前模型上下文压缩为已保存的检查点。 |
| `/model` | 选择此角色使用的模型和推理级别，并保存到会话及全局配置。 |
| `/model-session` | 选择当前模式的模型及推理级别，在 TUI 重置或恢复时保留，不更改全局默认值。 |
| `/skills` | 查看、启用、禁用和重新加载本地技能。 |
| `/ensemble-review <prompt>` | 收集独立 ACP 审阅意见并综合成只读审阅结果。 |

Build 和 Plan 各自独立保存提供商、模型及推理级别；切换模式、执行 `/new` 或
`/implement-fresh`（包括审批对话框中的等效操作）都会保留这两组选项。
新会话中的实施使用已保存的 **Build** 选项，而不是 Plan 的模型。`/new` 清空对话、
Plan 状态及已激活技能历史，不会自动请求模型，也不会重置模型偏好，即使配置默认值已改变。
独立启动的会话使用当前默认值；`/resume` 和 `--continue` 则恢复目标对话记录中的两组选项。
`/model-session` 从不写入配置；`/model` 还会保存所选模式的全局默认值。

### 从命令行恢复会话

```sh
zevria --continue
# 简写：
zevria -c
```

这两个命令都会恢复当前工作区中最近的会话。若要选择更早的会话，请在 UI 中使用 `/resume`。

## 项目指导与技能

将共享项目指令放在 `<workspace>/AGENTS.md`，将个人默认设置放在 `~/.zevria/AGENTS.md`。Zevria 会在打开或恢复会话时读取这些文件；项目指导优先于冲突的全局指导。它不会搜索祖先目录或嵌套目录；修改指导文件后，需要重新打开或恢复会话才会生效。平台和文件安全限制请参阅[指导文件说明](docs/guidance.md)。

技能将可复用的指令打包到以下任一位置：

- `~/.zevria/skills/`：全局技能。
- `<workspace>/.zevria/skills/`：项目技能。

使用 UI 中的 `/skills` 或命令行中的 `zevria skills list` 查看技能。使用 `$name` 而不是 `/name` 调用技能。关于普通 Markdown 文件、`SKILL.md` 包、元数据和管理命令，请参阅[技能指南](docs/skills.md)。

## 编辑器集成

ACP 客户端可以不启动终端 UI，直接运行 Zevria：

```sh
zevria --acp
```

请先完成 provider 配置。该命令会通过 stdin/stdout 提供以换行符分隔的 JSON-RPC 服务；它是 ACP 服务器，不是一次性提示 CLI。关于客户端集成、会话处理、能力和单独的 ensemble 工作智能体配置文件，请参阅 [ACP 智能体指南](docs/acp-agent.md)。

## 存储与安全

Zevria 使用当前用户的文件系统和进程权限执行工具。**它不提供操作系统级沙箱。** Plan/Explore 限制和委派工作区所有权都不构成安全隔离。ACP 前端不会在运行工具前向客户端请求许可。处理不可信工作时，请使用权限受限的操作系统账户、容器、虚拟机或沙箱。

工作区历史和生成的产物保存在 `.zevria/` 下，包括 `sessions/`、`subsessions/`、`ensemble-sessions/`、`agent-runs/` 和 `plans/`。日志默认保存在 `~/.zevria/logs/`。对话、工具输出和提交的图片可能包含敏感源材料；图片会以 Base64 形式嵌入纯文本历史记录并长期保存。请保护这些文件，勿将凭据提交到版本控制，并注意提示和相关工具输出会发送给配置的 provider。

`zevria clean` 会**不经确认就删除上面列出的五个工作区存储目录**。执行前请先停止所有使用该工作区的会话和工作智能体。它会保留技能和其他未列出的文件；它也不会彻底清除全局日志或 provider 保存的数据。

## 开发

在仓库根目录运行：

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
```

工作区主要目录：

| 目录 | 职责 |
| --- | --- |
| `crates/zevria` | CLI 入口和终端启动。 |
| `crates/app` | 配置、运行时组合和应用服务。 |
| `crates/core`、`crates/session-api` | 会话引擎以及前端/provider 契约。 |
| `crates/provider`、`crates/responses` | 模型路由和 Responses 传输/回放。 |
| `crates/acp`、`crates/ensemble` | ACP 前端和多智能体工作流。 |
| `crates/tui`、`crates/tui-input`、`crates/tui-widgets`、`crates/theme` | 终端界面、输入和呈现。 |
| 其他工作区 crate | 共享值、指令、模型、工作流、工具和对话记录。 |

关于 crate 边界和数据流，请参阅[架构说明](docs/architecture.md)。贡献代码时请遵循 [AGENTS.md](AGENTS.md)；所有更改都必须保留 provider 可缓存的提示前缀。

## 延伸阅读

- [Provider 配置与兼容性](docs/responses-compatible.md)
- [基于 ACP 的计划与审阅](docs/ensemble.md)
- [将 Zevria 作为 ACP 智能体运行](docs/acp-agent.md)
- [自动项目指导](docs/guidance.md)
- [本地技能](docs/skills.md)
- [图片输入与剪贴板行为](docs/image-input.md)
- [终端主题](docs/themes.md)
- [检查策略及其限制](docs/instructions/inspection-policy.md)
- [架构说明](docs/architecture.md)

## 友情链接

[Linux.Do](https://linux.do/) — 一个全新的理想社区。
