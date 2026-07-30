app-about = 交互式 C 语言 REPL
arg-cc = 指定 C 编译器（默认依次检查 $CC 和 PATH）
arg-std = 指定 C 语言标准，例如 c11、c17、c23（默认使用编译器设置）
arg-eval = 执行 CODE 后退出；可重复指定，失败时返回非零状态码
arg-script = 执行 FILE 中的输入后退出；任一输入失败时返回非零状态码
arg-quiet = 不显示启动横幅和提示
arg-timeout = 每次编译或程序运行允许的最长秒数（默认：10）
arg-no-color = 禁用彩色输出
arg-lang = 界面语言；覆盖系统 locale 自动检测（可选值：en、zh）
arg-web = 启动仅限本机访问的浏览器终端
arg-no-open = 不自动打开浏览器（需要同时指定 --web）
arg-help = 显示帮助
arg-version = 显示版本
cli-error = 错误：
cli-argument = 命令行参数
cli-invalid-value = {$argument} 的值“{$value}”无效
cli-unknown-argument = 无法识别参数 {$argument}
cli-equals-required = {$argument} 的值前需要等号
cli-wrong-value-count = {$argument} 的值数量不正确
cli-missing-value = {$argument} 缺少值
cli-argument-conflict = {$argument} 不能与 {$prior} 同时使用
cli-invalid-utf8 = 命令行参数不是有效的 UTF-8
cli-invalid-arguments = 命令行参数无效
cli-valid-values = 可选值：{$values}
cli-suggestion = 可能想使用：{$suggestion}
cli-usage = 用法：
cli-options = 选项：
cli-more-info = 更多信息请尝试“--help”。
script-open-error = 无法打开脚本 {$path}
startup-hint = 输入 C 代码进行求值 · 使用 %help 查看命令 · 按 Ctrl-D 退出
web-launch-hint = 提示：可使用 `c-shell --web` 启动浏览器界面
bye = 再见
web-listening = 浏览器终端：{$url}
web-stop-hint = 仅限本机访问 · 按 Ctrl-C 停止
web-open-failed = 无法自动打开默认浏览器；请手动打开上方网址
web-connecting = 正在连接…
web-disconnected = 连接已断开
web-session-failed = 无法启动 c-shell：{$error}

edit-interactive-only = %edit 仅可用于交互式 REPL 模式
external-side-effect-warning = 警告：检测到具有外部副作用的调用（{$calls}）；每次后续求值前都会重放保留的输入，因此输入操作以及文件或进程副作用可能重复发生
note-missing-semicolon = （已自动补上缺失的分号）
unprintable-value = 表达式有效，但此类值没有对应的打印器；已完成求值，但不显示 Out[n]
note-input-not-kept = （此输入未保留到会话中）
note-replaced-file = （已替换先前的文件作用域定义）
note-added-file = （已添加到文件作用域）
note-shadowed = （已打开嵌套作用域，以遮蔽先前的声明）
note-stdin-captured = （已捕获 {$count} 次标准输入请求用于重放；内容已隐藏）

edit-usage = 用法：%edit [输入编号]
edit-not-found = 没有编号为 In[{$number}] 的 C 输入
nothing-to-edit = 没有可编辑的输入
unicode-missing-expression = 缺少表达式
unicode-invalid-count = -n 需要非负的代码单元数量
unicode-count-limit = -n 最多允许 {$limit} 个代码单元
unicode-missing-after-count = -n <数量> 后缺少表达式

where-headers = 头文件：
where-header-column = 头文件
where-doc-column = 文档
where-not-found = 在 c-shell 的 ISO C 标准库索引中未找到 {$name}
where-name = 名称：
where-kind = 类别：
where-signature = 签名：
where-availability-range = ISO C 可用版本：{$since}–{$last}；在 {$removed} 中移除
where-availability-later = ISO C 可用版本：{$since} 及以后
where-selected-mode = 当前模式：
where-available = 可用
where-unavailable = 不能作为 ISO C 标准库标识符使用
where-auto-no = 由 c-shell 自动包含：否
where-auto-yes = 由 c-shell 自动包含：是
where-auto-conditional = 可用时由 c-shell 自动包含：
where-note = 说明：
kind-function = 函数
kind-function-like-macro = 函数式宏
kind-object-like-macro = 对象式宏
kind-type-generic-macro = 泛型选择宏
kind-typedef = 类型定义
kind-type = 类型
index-note-gets = 已弃用；由于无法限制输入长度，已在 ISO C11 中移除
index-note-stdbool = 截至 C17 由 <stdbool.h> 提供；在 C23 中是语言关键字
index-note-stdalign = 截至 C17 由 <stdalign.h> 提供；在 C23 中是语言关键字
index-note-assert = 在 C11/C17 中由 <assert.h> 提供；在 C23 中是语言关键字
index-note-obsolescent = 在 C23 中已过时
index-note-noreturn = C23 中仍由 <stdnoreturn.h> 提供；新 C23 代码应优先使用 [[noreturn]]
index-note-macro-form = 实现还可能额外提供宏形式

magic-help =
    命令：
      %help [--verbose]  显示命令；--verbose 额外显示使用说明
      %quit / %exit      退出（也可按 Ctrl-D）
      %clear             清屏，但不改变会话
      %reset             清空并重新开始会话
      %src [--raw]       显示用户 C 代码；--raw 包含生成的运行时与协议
      %header            列出每个程序都会包含的头文件
      %edit [n]          将最近一次或 In[n] 输入复制到编辑区
      %type <表达式>     查询表达式类型，但不求值
      %bits <表达式>     使用小写十六进制查看标量值
      %Bits <表达式>     与 %bits 相同，但使用大写十六进制
      %utf8/%utf16/%utf32 [-n N] <表达式> 解码 Unicode 代码单元
      %where <标识符>    查询 ISO C 标准库标识符所属的头文件
      %time <代码...>    执行一次并测量语句或表达式
      %timeit <代码...>  多轮循环测量语句或表达式
      %cc [路径]         显示或切换 C 编译器
      %std [标准]        显示或切换语言标准（c11/c17/c23）；
                         %std default 恢复编译器默认标准

magic-help-notes =
    说明：
      裸表达式会显示值；末尾添加“;”会静默执行。
      完整的 if 会等待一个空白续行；可在该行输入 else 或 else if 继续。
      其他已闭合代码块会立即提交，但 struct/union/enum 定义会等待必须的“;”。
      函数定义、#include 和 typedef 会自动放到文件作用域。
      %edit n 可重新打开本会话中的任意 C 输入 In[n]，包括失败的输入；
      它只填充下一提示符。修改后按 Enter 会以新编号提交，原输入保持不变。
      c-shell 已提供 main()；请直接输入函数体中的语句，并省略最后的 return。
      重新声明局部变量会打开嵌套遮蔽作用域。重新定义函数或类型时，只有完整
      重写后的会话能被编译器接受，才会替换先前的文件作用域输入；函数绝不会
      被降级到 main 内。
      %type 使用 _Generic 匹配：可命名标量类型及其指针；完整的命名
      struct/union 会显示 Struct Point 或 Union Value；简单匿名 typedef
      使用 typedef 名称。其他别名和顶层限定符会被规范化，数组与函数执行
      正常的表达式转换。
      %bits/%Bits 对标量表达式求值一次，显示类型、值、大小、十六进制和
      二进制表示、内存字节与字节序。命令区分大小写；只有 %Bits 使用大写
      十六进制。IEEE-754 float/double 还会显示符号、指数和尾数字段；
      不支持聚合、数组和函数指针。
      %utf8/%utf16/%utf32 将整数数组或指针按 Unicode 代码单元读取。
      默认在 NUL 或 100 个单元处停止；-n N 精确读取 N 个单元，上限 4096。
      无效 Unicode 会被报告且不插入替换字符。显式读取指针时，地址仍须有效。
      %where 使用内置 ISO C89-C23 索引，而非宿主头文件的传递可见性，并链接
      到 cppreference。它不包含 POSIX、平台/编译器扩展或用户名称。
      %time 执行表达式或语句一次并显示输出/值，只测量该输入在 C 中的执行；
      不包含编译、进程启动和历史会话重放。副作用会保留到会话。
      %timeit 自动选择循环次数并进行多轮测量，不修改会话状态；可能改变状态
      的输入会收到警告，因为它将被重复执行。
      struct 值按成员显示；已知的嵌套 struct 和数组会展开，但指针成员只显示
      地址或 NULL。可使用显式成员表达式（p.name）或解引用（*ptr）继续查看。
      数组最多展开一到二维，每维上限 100；真实指针仍显示地址，没有打印器的
      元素显示 <unprintable>。
      纯裸表达式（x + 1、sizeof(int)）求值后丢弃；语句以及可能产生效果的
      裸表达式会被保留。
      直接 scanf 调用会为每次动态请求记录一行私有输入，后续重放使用该记录，
      包括函数和循环中的调用。其他已知文件/输入/进程 API 会因外部效果可能
      重复而发出警告。

help-usage = 用法：%help [--verbose]
session-cleared = 会话已清空
headers-intro = 自动包含（可选头文件带有可用性保护）：
src-usage = 用法：%src [--raw]
type-usage = 用法：%type <表达式>
type-no-result = 类型查询没有产生结果
bits-usage = 用法：%{$command} <表达式>
bits-unsupported = %{$command} 支持标准标量值以及指向标量类型的指针
bits-no-result = 位表示查询没有产生结果
bits-type = 类型：
bits-size = 大小：
bits-byte = 字节
bits-bytes = 字节
bits-value = 值：
bits-hex = 十六进制：
bits-binary = 二进制：
bits-memory = 内存：
bits-byte-order = 字节序：
bits-little-endian = 小端
bits-big-endian = 大端
bits-sign = 符号：
bits-exponent = 指数：
bits-fraction = 尾数：
bits-zero-subnormal = 零/次正规数
bits-infinity-nan = 无穷/NaN
unicode-usage = 用法：%{$command} [-n 代码单元数] <表达式>（{$message}）
unicode-unsupported = %{$command} 支持整数代码单元类型的指针和数组
unicode-no-result = Unicode 查询没有产生结果
where-usage = 用法：%where <标识符>
time-usage = 用法：%time <表达式或语句>
wall-time = 实际用时：
wall-time-unavailable = 实际用时：不可用（输入未完成）
timeit-usage = 用法：%timeit <表达式或语句>
timeit-state-warning = “%timeit 输入”可能被重复执行，且不会保留用于重放；后续求值不包含它对 C 状态的修改
std-unsupported = 此编译器不支持 -std={$standard}
unknown-command = 未知命令 %{$command} — 请尝试 %help

temp-source-write-error = 无法写入临时源文件：{$error}
temp-dir-error = 无法创建临时目录
compiler-run-error = 无法运行编译器：{$error}
compiler-output-truncated = 编译器每个输出流超过 {$mib} MiB，已截断
compiler-timeout = 编译器在 {$seconds} 秒后超时并被终止
main-already-provided = c-shell 已提供 main()；请直接输入函数体中的语句，并省略最后的 return
program-start-error = 无法启动 {$path}
program-killed = 已在 {$seconds} 秒后终止（可能存在无限循环）
program-output-truncated = 程序每个输出流超过 {$mib} MiB，已截断
stdin-tape-diverged = 重放保留输入时，标准输入记录出现分歧；请使用 %reset
program-exited-early = 程序在输入完成前退出
timeit-run = 轮
timeit-runs = 轮
timeit-loop = 次循环
timeit-loops = 次循环
timeit-report = 每次循环 {$mean} ± {$deviation}（{$runs} {$run-word}的平均值 ± 标准差，每轮 {$loops} {$loop-word}）

unicode-code-units = 代码单元：
unicode-encoding = 编码：
unicode-address = 地址：
unicode-width-error = 错误：预期 {$expected} 字节代码单元，但表达式指向 {$actual} 字节元素
unicode-text = 文本：
unicode-text-prefix = 文本前缀：
unicode-no-nul = 说明：前 {$limit} 个代码单元中没有 NUL 终止符
unicode-invalid-utf8-unit = 索引 {$index} 处的 UTF-8 代码单元无效
unicode-invalid-utf8-sequence = 从代码单元 {$index} 开始的 UTF-8 序列无效
unicode-invalid-utf32 = 索引 {$index} 处的 UTF-32 标量值无效
unicode-invalid-utf16-unit = 索引 {$index} 处的 UTF-16 代码单元无效
unicode-unpaired-high = 索引 {$index} 处的 UTF-16 高代理项未配对
unicode-unpaired-low = 索引 {$index} 处的 UTF-16 低代理项未配对
array-more = ...（另有 {$count} 项）

signal-segv = 程序崩溃：段错误（SIGSEGV）——通常由 NULL/野指针解引用或数组越界引起
signal-abrt = 程序中止（SIGABRT）——通常由断言失败或 C 库检测到堆损坏引起
signal-fpe = 算术错误（SIGFPE）——通常由整数除以零引起
signal-ill = 非法指令（SIGILL）
signal-bus = 总线错误（SIGBUS）——通常由未对齐的内存访问引起
signal-other = 程序因信号 {$signal} 终止
windows-access-violation = 程序崩溃：访问冲突——通常由 NULL/野指针解引用或数组越界引起
windows-division-zero = 算术错误：整数除以零
windows-illegal-instruction = 非法指令
windows-stack-overflow = 栈溢出——通常由失控递归或过大的栈数组引起
windows-buffer-overrun = 检测到栈缓冲区溢出

compiler-cannot-build = {$path}（无法构建可运行的程序）
compiler-standard-unsupported = {$path}（不支持请求的标准 {$standard}）
compiler-printer-unsupported = {$path}（当前模式无法编译值打印器）
compiler-not-found =
    未找到可用的 C 编译器（已尝试：{$tried}）。
    c-shell 需要能够编译其 C11 风格值打印器的语言模式。
    请安装 gcc 或 clang，或使用 --cc <路径> 指定编译器。{$windows-note}
compiler-windows-note =
    在 Windows 上，MSVC（cl.exe）只能从 Developer Command Prompt 中使用。
compiler-default-std = 默认标准
compiler-default-std-value = 默认标准 {$standard}
compiler-auto-std = -std={$selected}，已自动提升：默认模式 {$default} 不支持 _Generic
