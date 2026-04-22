# Troubleshooting

Here are some common debugger caveats. If you encounter other problems, 
please create an issue [here](https://github.com/software-mansion/cairo-debugger/issues),
or contact us via [Telegram channel](https://t.me/starknet_foundry_support).

## The "▶ Debug Test" lens is not visible

1. Make sure that you have followed [the installation instructions](/docs#installation) - especially that 
versions of the installed tools are correct.
2. Reload VSCode (`Ctrl+Shift+P` → `Developer: Reload Window`) if the lens does not appear after installation.

## A breakpoint is hollow

A hollow breakpoint means the debugger determined that the line **cannot be hit** during execution. Such a breakpoint is called
an **unverified breakpoint**.

This is a result of an optimization applied during Sierra to CASM compilation that cannot be disabled.

> [!NOTE]
> Cairo compiles down to Sierra, which compiles to CASM.
> Due to optimizations of Sierra to CASM compiler, certain sierra statements have no corresponding CASM instructions.
> Since Cairo Debugger relies on Cairo VM - which executes CASM instructions -
> Cairo source code lines that produce such Sierra statements cannot be a valid breakpoint.

## Some variables are not shown

Currently, the debugger only supports simple numeric variables than can fit within single `felt252`.

Support for complex types and arrays is in progress - you can track it [here](https://github.com/software-mansion/cairo-debugger/issues/101) and [here](https://github.com/software-mansion/cairo-debugger/issues/103).

## Contract calls are skipped

Currently, the debugger does not enter contract calls. 
Since each contract call is executed in a separate VM, some architectural changes are required to support it.

Support for contract calls is in progress - you can track it [here](https://github.com/software-mansion/cairo-debugger/issues/102).
