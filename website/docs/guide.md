# Debugging in VSCode

This guide walks you through setting up and using Cairo Debugger in Visual Studio Code - the primary supported environment.

## Prerequisites

Before you start, make sure you have:

- [**snforge**](https://foundry-rs.github.io/starknet-foundry/getting-started/installation.html) `>=0.60.0`
- [**Scarb**](https://docs.swmansion.com/scarb/download.html) `>=2.18.0`
- The lastest [**Cairo 1.0**](https://marketplace.visualstudio.com/items?itemName=starkware.cairo1) VSCode extension `>=3.6.0`

Also, complete the [Scarb.toml configuration](../docs#prerequisites) from the quick start before continuing.

## Step 1 - Open your project in VSCode

Open the root folder of your Cairo project (the one containing `Scarb.toml`) with **File → Open Folder**.

## Step 2 - Set breakpoints

Click in the gutter (the space to the left of the line numbers) on any Cairo source line where you want execution to pause. A red dot confirms the breakpoint is set.

<img height="400" src="/Breakpoints.png" title="Breakpoints" width="400"/>

> [!TIP]
> You can set multiple breakpoints before launching. The debugger will stop at each one in order.

## Step 3 - Launch the debugger

The [Cairo Language Server](https://docs.swmansion.com/cairols/) - which is distributed with Scarb - 
adds a **▶ Debug Test** code lens above every test function. Click it to start a debugging session.

<img height="200" src="/DebuggerLens.png" title="Debugger lens" width="200"/>

## Step 4 - Navigate the debugger

Once execution hits a breakpoint, use the standard VSCode debug toolbar:

| Action | Keyboard shortcut | Icon | Description |
|---|---|---|---|
| Continue | `F5` | <img height="50" src="/Continue.png" title="Continue" width="50"/> | Resume until the next breakpoint |
| Step Over | `F10` | <img height="50" src="/StepOver.png" title="Step Over" width="50"/> | Execute the current line and advance |
| Step Into | `F11` | <img height="50" src="/StepInto.png" title="Step Into" width="50"/> | Step into the function call on the current line |
| Step Out | `Shift+F11` | <img height="50" src="/StepOut.png" title="Step Out" width="50"/> | Run until the current function returns |
| Restart | `Cmd+Shift+F5` / `Ctrl+Shift+F5` | <img height="50" src="/Restart.png" title="Restart" width="50"/> | Restart the debugging session |
| Stop | `Shift+F5` | <img height="50" src="/Stop.png" title="Stop" width="50"/> | Terminate the session |

Check [here](https://code.visualstudio.com/docs/debugtest/debugging) for details on how to interact with the VSCode UI.

## Step 5 - Inspect variables and call stack

While paused, the **Variables** panel in the **Run and Debug** view shows current values of local variables of the current function frame.
Similarly, the **Call Stack** panel shows function frames that are currently on the stack.

<img height="800" src="/Variables.png" title="Variables" width="800"/>

You can click on different function frames to inspect variables of these function frames.

<img height="800" src="/VariablesDifferentFrame.png" title="Variables for a different frame" width="800"/>
