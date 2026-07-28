<h1 align="center">c-shell</h1>

<p align="center">
  <a href="README.md">English</a>
  ·
  简体中文
</p>

<p align="center">
  <strong>由真实编译器驱动的 C 语言交互式 Shell。</strong>
  <br>
  无需临时编写 <code>main</code>，即可探索 C 语法、类型、诊断与具体实现行为。
</p>

<p align="center">
  <a href="https://crates.io/crates/c-shell"><img alt="crates.io 版本" src="https://img.shields.io/crates/v/c-shell?style=flat-square&logo=rust&label=crates.io"></a>
  <a href="https://github.com/ChenZhongPu/C-Shell/actions/workflows/ci.yml"><img alt="CI 状态" src="https://img.shields.io/github/actions/workflow/status/ChenZhongPu/C-Shell/ci.yml?branch=main&style=flat-square&logo=githubactions&logoColor=white&label=CI"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust 1.96 或更高版本" src="https://img.shields.io/badge/Rust-1.96%2B-000000?style=flat-square&logo=rust"></a>
  <a href="https://github.com/ChenZhongPu/C-Shell/releases"><img alt="Linux、macOS 和 Windows" src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-4C566A?style=flat-square"></a>
  <a href="LICENSE"><img alt="MIT 许可证" src="https://img.shields.io/badge/license-MIT-2F80ED?style=flat-square"></a>
</p>

<p align="center">
  <a href="#快速体验">快速体验</a>
  ·
  <a href="#安装">安装</a>
  ·
  <a href="#使用方式">使用方式</a>
  ·
  <a href="#magic-命令">Magic 命令</a>
  ·
  <a href="#工作原理">工作原理</a>
  ·
  <a href="#已知限制">已知限制</a>
</p>

<p align="center">
  <img src="demo.gif" alt="c-shell 交互式会话演示" width="900">
</p>

> [!WARNING]
> 本项目是 C 编程语言的 REPL，与 Unix Shell
> [C shell（`csh`）](https://en.wikipedia.org/wiki/C_shell)无关。

## 快速体验

启动 `c-shell --lang zh`，然后直接在提示符中输入 C 代码：

```text
c-shell 0.2.8  ·  cc (GCC) 16.1.1 (默认标准 gnu23)
In [1]: int x = 41;
In [2]: x + 1
Out[2]: 42
In [3]: 3 / 2
Out[3]: 1
In [4]: 3.0 / 2
Out[4]: 1.5
In [5]: -1 > 0u
<input>:1:4: warning: comparison of integer expressions of different signedness
    1 | -1 > 0u
      |    ^
Out[5]: 1
```

编译器诊断会原样显示，因此上例中的 GCC 警告仍然是英文；c-shell 自己生成的帮助、
提示和错误则会根据界面语言显示。

## 主要特点

- **由你的编译器给出答案。** GNU GCC、LLVM Clang、Apple Clang 和 MSVC
  决定语言规则、ABI 行为、警告与错误。
- **有状态的 C 会话。** 声明、语句、函数和类型可供后续输入使用，同时保留
  C 本身的作用域规则。
- **适合交互输入。** 提供语法高亮、补全、会话内历史、智能续行缩进、右花括号
  自动减少缩进，以及对先前输入的重新编辑。
- **直接显示有用的值。** 自动打印标量、字符串、数组和受支持的结构体；
  `%type` 可在不求值的情况下查询类型，`%bits`/`%Bits` 可查看对象表示和
  IEEE-754 字段。
- **Unicode 检查工具。** `%utf8`、`%utf16` 和 `%utf32` 可显式解码 Unicode
  代码单元。
- **标准库索引。** `%where` 可查询 ISO C 标准库名称所属的标准头文件及其
  标准版本信息。
- **既可交互，也可编写脚本。** 支持 REPL、`-e`、脚本文件和管道输入，并提供
  确定的退出状态。
- **跨平台编译器驱动。** 通过能力探测识别 GNU 风格与 MSVC 风格的命令行，
  不依赖可执行文件名称猜测。

## 为什么使用真实编译器？

c-shell 会把当前输入与会话状态重新组装成完整 C 程序，然后交给机器上的
**真实 C 编译器**。这样得到的整数提升、实现定义行为、ABI 细节和编译器诊断，
都来自你实际使用的工具链，而不是某个解释器对 C 的近似模拟。

一次求值结果仍然只是某次具体编译和执行的观察结果，并不等同于 C 标准保证。
未定义行为或未指定行为可能随着编译器版本、参数、周围代码或另一次执行发生变化。

## 安装

构建需要 Rust 工具链；运行时需要 GNU GCC、LLVM Clang、Apple Clang 或
MSVC。其他编译器不在当前支持和测试范围内。

从 crates.io 安装：

```sh
cargo install c-shell
```

从源码目录安装：

```sh
cargo install --path .
```

使用 `--cc` 时只尝试指定的编译器。否则，c-shell 会先检查 `$CC`，再检查
PATH：Unix 上依次尝试 `cc`、`gcc`、`clang`；Windows 上尝试 `gcc`、
`clang`、`cc`、`clang-cl`、`cl`。

`--cc` 和 `$CC` 必须是单个可执行文件名称或路径，不能是包含参数的 Shell
命令。c-shell 会通过实际编译探测能力，而不是从版本字符串推断。例如 macOS
中的 `/usr/bin/gcc` 可能实际是 Apple Clang 驱动，而 Homebrew 或 MacPorts
安装的 GCC 才是 GNU GCC。

常用启动参数：

```sh
c-shell                              # 自动检测编译器
c-shell --cc clang --std c23         # 指定编译器与语言标准
c-shell --timeout 30                 # 每次编译或程序运行最多 30 秒
c-shell --lang zh                    # 强制使用中文界面
c-shell --lang en                    # 强制使用英文界面
c-shell -e 'sizeof(long)'            # 求值后退出
c-shell --script demo.csh            # 执行脚本文件
c-shell --quiet                      # 交互模式但不显示横幅
echo '1 + 1' | c-shell               # 从管道读取输入
```

终端处理是自动的：标准输出不是终端、`TERM=dumb` 或设置了 `NO_COLOR` 时，
颜色会关闭；标准输入不是终端时，不显示横幅、提示符与告别信息，并以批处理模式
读取输入。批处理模式同样会累积多行函数和控制结构。

界面默认使用英文；检测到中文系统 locale 时自动切换为中文。`--lang en` 和
`--lang zh` 可覆盖自动检测。语言设置只影响 c-shell 自己产生的文本：
编译器诊断、编译器版本信息和用户 C 程序的输出始终原样传递。

语言标准默认跟随**编译器自身的默认模式**，例如 GCC 16 可能是 gnu23，
Clang 22 可能是 gnu17，实际结果会显示在启动横幅中。使用 `--std c17`
可以在启动时指定标准，也可以在会话中使用 `%std c17` 或 `%std default`。

有一个例外：如果编译器默认模式不能编译 `_Generic`，c-shell 会依次尝试
c17 和 c11，并在横幅中说明自动提升。`_Generic` 是值打印器所需的最低能力；
无法在所选模式中编译代表性值打印器的编译器不会被接受。

## 使用方式

| 输入 | 行为 |
| --- | --- |
| `x + 1` | 求值并显示为 `Out[n]` |
| `x + 1;` | 末尾带 `;`，静默执行 |
| `int x = 41;` | 声明变量，后续输入可见 |
| `int f(int a) { ... }` | 定义函数，并放到文件作用域 |
| `int main() { ... }` | 拒绝并给出说明；`main` 由 c-shell 提供 |
| `#include <time.h>` | 自动放到文件作用域 |

完成的交互式 `if` 会等待一个空白续行：直接按 Enter 提交，或输入 `else` /
`else if` 继续同一条语句。函数和循环在所需闭合语法完成后立即提交。
带花括号的 `struct`、`union` 和 `enum` 定义会继续等待声明末尾必需的分号。
跨行控制语句、`do ... while` 和直到 `#endif` 的条件预处理组都会自动累积。

c-shell 会生成自己的 `main`，用于承载会话局部变量和值输出协议。因此直接粘贴
`int main() { puts("hi"); }` 不会交给编译器产生晦涩的重复定义或嵌套函数警告，
而是明确提示：直接输入函数体中的语句，并省略最后的 `return`。

常用头文件 `stdio`、`stdlib`、`string`、`math`、`stdbool`、`stdint`、
`inttypes`、`stddef`、`limits`、`ctype`、`stdarg`、`time` 和 `wchar`
会预先包含。只有当编译器确认宿主 C 库提供 `<uchar.h>` 时才会包含 `uchar`；
部分 macOS SDK 并不提供该头文件。使用 `%header` 可以查看准确的受保护
include 块。

Unix 构建会链接 `-lm`；Windows 数学函数来自 C 运行库。GNU 风格的
GCC/Clang 驱动使用 `-Wall -Wextra`，MSVC 风格的 `cl`/`clang-cl`
使用 `/W3`。

### 重复声明与重新定义

块作用域声明因重复声明诊断失败时，c-shell 会在新的嵌套块中重试。如果完整程序
能够编译，后续输入会留在该块内并看到新的遮蔽声明：

```text
In [1]: int x = 1;
In [2]: x = 5;
In [3]: int x = 2;
（已打开嵌套作用域，以遮蔽先前的声明）
In [4]: x
Out[4]: 2
```

`%src` 会显示实际生成的花括号；c-shell 不会把声明改写为赋值。因此 C 自身的
作用域规则仍然成立，例如 `int x = x + 1;` 中的初始化器引用新声明的 `x`，
而不是外层变量。

文件作用域的重新定义采用另一种方式：c-shell 会尝试用新函数或类型替换先前的
文件作用域输入，只有重写后的完整会话能被真实编译器接受时才提交：

```text
In [1]: int f(int n) { return n * 2; }
（已添加到文件作用域）
In [2]: f(3)
Out[2]: 6
In [3]: int f(int n) { return n * 3; }
（已替换先前的文件作用域定义）
In [4]: f(3)
Out[4]: 9
```

旧定义会在原位置被替换，以保持声明顺序。形似函数的输入绝不会被降级到
`main` 内部，这可以避免 GCC 嵌套函数扩展产生 Clang 或 MSVC 无法解释的会话。
`%reset` 会清除整个会话。

### Magic 命令

Magic 命令使用 `%` 前缀，风格与 IPython 类似：

```text
%help      %quit      %clear     %reset     %edit [n]
%src       %header    %where     %type      %bits/%Bits
%utf8      %utf16     %utf32     %time      %timeit
%cc        %std
```

`%help` 只显示一屏命令摘要；`%help --verbose` 会追加详细使用说明，包括哪些输入
会被保留、续行与遮蔽规则、值打印器覆盖范围以及 `scanf` 输入记录的重放方式。

`%time` 执行指定输入一次，并且只测量该输入在生成的 C 进程中的执行时间；
编译、进程启动和历史会话重放不计入。`%timeit` 采用类似 IPython 的模型：
自动选择循环次数并报告多轮统计结果。

`%clear` 清除终端显示并把光标移到顶部，但不改变变量、保留的 C 代码或输入编号。

#### UTF-8 字面量预览

直接的 `u8"..."` 字面量，以及明确声明为一维 `char8_t` 数组的标识符，会得到
经过验证的 UTF-8 预览：

```text
In [1]: const char8_t smiley[] = u8"\U0001F642";
In [2]: smiley
Out[2]: u8"🙂"
代码单元： {0xf0, 0x9f, 0x99, 0x82, 0x00}
```

结尾的零会保留在代码单元列表中，但不会出现在引号文本中。C23 把 `char8_t`
定义为与 `unsigned char` 相同的类型，因此运行时类型系统无法区分两者的源码
拼写。c-shell 只对明确的 `u8` 前缀或保留的 `char8_t[]` 声明显示文本预览；
普通 `unsigned char[]`、指针、多维数组和复杂表达式仍按数字显示。无效或不完整
的 UTF-8 也会回退为数字代码单元，而不是插入替换字符。

#### `%utf8`、`%utf16` 与 `%utf32`

这些命令把整数数组或指针显式解释为 Unicode 代码单元：

```text
In [4]: const char8_t *message = u8"A好😀";
In [5]: %utf8 message
编码： UTF-8
地址： 0x55ee74fe4032
文本： u8"A好😀"
代码单元： {0x41, 0xe5, 0xa5, 0xbd, 0xf0, 0x9f, 0x98, 0x80, 0x00}

In [5]: %utf16 u"A\u597D\U0001F600"
编码： UTF-16
地址： 0x55ee74fe4070
文本： u"A好😀"
代码单元： {0x0041, 0x597d, 0xd83d, 0xde00, 0x0000}
```

默认形式在 NUL 处停止，并最多读取 100 个代码单元。`-n N` 会精确读取 `N`
个单元（包括内嵌零），上限是 4096：

```text
In [5]: %utf8 -n 3 (unsigned char[]){'A', 0, 'B'}
编码： UTF-8
地址： 0x7ffd12345678
文本： u8"A\0B"
代码单元： {0x41, 0x00, 0x42}
```

`N` 表示代码单元数量，因此 UTF-8 中按字节计数，UTF-16 中按 16 位单元计数，
UTF-32 中按 32 位单元计数。指向元素的宽度必须与所选编码匹配。UTF-8 会严格
验证，UTF-16 会检查代理项配对，UTF-32 只接受 Unicode 标量值。无效数据会
报告对应代码单元索引，不会替换为 `�`。

指针检查必须读取目标地址。数量限制可以约束扫描与输出，但无法让悬空指针、
长度不足或其他无效指针变得安全；使用 `-n` 时应确保指定数量的单元可读。

#### `%src` 与 `%edit`

`%src` 默认显示面向用户的程序：当前文件作用域定义，以及干净 `main` 中保留的
语句和遮蔽花括号，不包含打印器或协议标记。`%src --raw` 显示包含 `CS_PRINT`、
`_Generic` 和标记协议的完整编译器输入。

如果系统中存在 `clang-format`，两种视图都会用它进行展示格式化；格式化仅影响
显示，具有三秒超时，实际求值仍使用未格式化的生成源码。

`%edit` 把最近一次 C 输入复制回终端编辑区；`%edit 12` 取回 `In[12]`。
成功、失败以及求值后忘记的纯查询都可以在 `%reset` 前按编号取回。命令本身不会
编译、提交或消耗输入编号；修改后按 Enter 才会作为新的 `In[n]` 提交。
`%edit` 仅适用于交互式 REPL。

#### `%type`

`%type <表达式>` 查询表达式类型，但不求值、不提交代码，也不消耗输入编号：

```text
In [1]: const char *message = "hello";
In [2]: %type message
const char *
In [2]: %type 1 + 0.5
double
```

聚合类型会尽可能保留名称：

```text
In [2]: struct Point { int x, y; } point;
（已添加到文件作用域）
In [3]: %type point
Struct Point
In [3]: typedef union { int i; double d; } Value;
（已添加到文件作用域）
In [4]: Value value = { 1 };
In [5]: %type value
Union Value
```

实现使用 C11 `_Generic`，而不是编译器特有的 `typeof`。它可识别标量类型、
标量指针、会话中完整的命名聚合定义，以及简单的匿名聚合 typedef。真正匿名且
不在匹配表中的类型会显示 `<unrecognized type>`。

#### `%bits` 与 `%Bits`

`%bits <表达式>` 对标量表达式求值一次，并显示所选编译器与目标产生的对象表示：

```text
In [1]: %bits -1
类型： int
大小： 4 字节
值： -1
十六进制： 0xffffffff
二进制： 11111111 11111111 11111111 11111111
内存： ff ff ff ff
字节序： 小端

In [1]: %bits 0.1f
类型： float
大小： 4 字节
值： 0.100000001
十六进制： 0x3dcccccd
二进制： 00111101 11001100 11001100 11001101
内存： cd cc cc 3d
字节序： 小端
符号： 0
指数： 123 (-4)
尾数： 0x4ccccd
```

`%Bits` 执行相同检查，但使用大写十六进制数字和前缀：

```text
In [1]: %Bits 0.1f
十六进制： 0X3DCCCCCD
内存： CD CC CC 3D
尾数： 0X4CCCCD
```

Magic 命令区分大小写：只识别 `%bits` 和 `%Bits`，`%BITS` 无效。

它支持标准整数、布尔、字符和浮点类型，通过兼容整数类型支持枚举，也支持指向
标量类型的指针。`hex` 与 `binary` 按数值有效位顺序显示，`memory` 保持地址
递增顺序，因此可以直接观察字节序。常见 IEEE-754 binary32/binary64 目标上的
`float` 和 `double` 还会显示符号、指数与尾数字段。

#### `%where`

`%where <标识符>` 查询 ISO C 标准头文件、标识符类别、标准可用版本以及当前
语言模式状态，不会编译或改变会话：

```text
In [1]: %where gets
名称： gets
类别： 函数
头文件：
+-----------+--------------------------------------------+
| 头文件    | 文档                                       |
+-----------+--------------------------------------------+
| <stdio.h> | https://en.cppreference.com/c/header/stdio |
+-----------+--------------------------------------------+
签名： char *gets(char *s)
ISO C 可用版本：C89–C99；在 C11 中移除
当前模式： gnu23 (不能作为 ISO C 标准库标识符使用)
由 c-shell 自动包含：是 (<stdio.h>)
说明：已弃用；由于无法限制输入长度，已在 ISO C11 中移除
```

内置索引覆盖 ISO C89 到 C23 中常见的可移植公开名称。它不会根据宿主机器上
头文件的传递包含关系猜测归属，因为这些关系和扩展在不同平台上并不一致。
每个匹配头文件都会附带 cppreference 链接；支持 OSC 8 的终端可直接点击，
不支持的终端和捕获输出仍能看到完整 URL。

POSIX 名称（例如 `getline`）、编译器或平台扩展、可选边界检查接口和用户声明
不会进入索引。对于具体实现的可用性、feature-test 宏和额外警告，本地
`man 3` 页面仍然有价值；例如 `man 3 gets` 会明确标记 `gets` 已弃用。

### 编辑与补全

Tab 会补全 Magic 命令、C 关键字、常用标准库名称，以及长度至少为两个字符的
会话标识符。前缀有多个匹配项时，会在光标下方打开下拉菜单；Tab 和方向键用于
移动，Enter 接受候选项。

上/下方向键可回忆当前进程中的最多 1000 条输入，因此多行代码块也可以恢复。
当前不会从磁盘加载或保存历史，没有 `%history` 命令或历史文件。供 `%edit n`
使用的编号输入档案同样只存在于当前会话中，并由 `%reset` 清除。

## 工作原理

c-shell 会重新组装会话，并把完整 C 文件交给真实宿主编译器
（GNU GCC、LLVM/Apple Clang 或 MSVC）。

关于输入累积与重放、`scanf` 标准输入记录、`_Generic` 值打印器、诊断位置映射
和能力缓存的技术细节，请阅读
**[HOW_IT_WORKS.md](HOW_IT_WORKS.md)**。

## 已知限制

关于非沙箱执行、标准输入重放范围、状态模型边界和平台行为的完整说明，请阅读
**[LIMITATIONS.md](LIMITATIONS.md)**。

## 开发

克隆仓库后，可以启用与 CI 相同的提交前检查（fmt、Clippy 和测试）：

```sh
git config core.hooksPath .githooks
```

`rust-toolchain.toml` 固定了 Rust 工具链，因此本地与 CI 使用相同版本的
Clippy。修改代码前建议阅读 [DESIGN.md](DESIGN.md)，其中记录了架构决策、
状态模型问题以及修改时容易踩到的兼容性陷阱。

界面翻译使用 Fluent，资源位于 `locales/`。英文是回退语言，测试会要求所有
语言资源拥有相同的消息键。不要把编译器诊断和用户程序输出加入翻译资源：
它们分别属于所选编译器和用户的 C 程序。

## 许可证

MIT
