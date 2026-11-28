# Factory

An agent software factory on one Linux workstation: it runs coding agents unattended, paces them against their plans' allowances, and lends them the PC's phones, emulators and local stacks.

## Language

### Usage

**Harness**:
An agent CLI the factory runs work through: Claude Code, Codex or agy.
_Avoid_: model, tool, agent (for the CLI itself)

**Usage window**:
One rolling allowance of a harness's plan, such as its 5-hour or its weekly window, with a share used and a reset time.
_Avoid_: usage pool, limit, bucket

**Quota**:
A harness's standing across its usage windows: available, held, or cooling after a refusal.
_Avoid_: usage pool, limit, credits

**Quota group**:
One agy model group (Gemini models; Claude and GPT models), which has usage windows of its own.
_Avoid_: tier, bucket

### Lab

**Lab**:
This PC's phones, emulators and local stack slots, which the factory lends to one holder at a time.
_Avoid_: devices, devlab (that is the command)

**Readiness**:
Whether a plugged-in phone can run unattended: ready, locked, asleep, stay-awake off, battery faked (a test's `dumpsys battery` fake left on), unauthorized, offline, or USB debugging off.
_Avoid_: phone state, health

**CI headroom**:
What the CI runners' systemd slice may still grow by: its `memory.max` less its `memory.current`. A grant counts only the memory free after it.
_Avoid_: CI reserve, runner memory

### Display

**Bar feed**:
The one document the factory gives the status bar: each harness's quota, the factory's work and the lab at a glance.
_Avoid_: cockpit feed, cockpit
