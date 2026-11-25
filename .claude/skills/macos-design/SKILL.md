---
name: macos-design
description: macOS menu-bar app design language for Controlz - Human Interface Guidelines essentials, layout and spacing tokens, native control choices, states, accessibility and the design spec template. Used by the designer to write specs and by the implementer when building UI.
user-invocable: false
---

# macOS menu-bar design

## Principles
1. **Glanceable first.** Opening the popover answers "is it on, what temperature, what volume?" in under a second, before any interaction.
2. **One click for the frequent things.** Power, temperature ±, volume ±, mute, mode. Everything else is secondary or in Settings.
3. **Native, not custom.** Standard SwiftUI controls, system fonts, SF Symbols, system colors and materials. It should look like Control Center, not a web page.
4. **Honest latency.** The AC goes through the cloud (seconds); the TV is local (instant). Show pending state; never pretend a command succeeded before confirmation.
5. **Quiet.** No alerts for routine failures; show inline status. Use alerts only for things that need a decision.

## Layout tokens
| Token | Value |
|---|---|
| Popover width | 300–320 pt |
| Outer padding | 12 pt |
| Section spacing | 12 pt |
| Row spacing | 8 pt |
| Control corner radius | system default (don't override) |
| Large value (temperature/volume) | `.system(size: 28, weight: .semibold, design: .rounded)`, `monospacedDigit()` |
| Section title | `.headline` |
| Secondary text | `.subheadline`, `.secondary` foreground |
| Background | `.regularMaterial` / default window material |

## Controls
- Power: `Toggle` with `.switch` style, or a circular SF Symbol button `power` if it sits in a header.
- Temperature: stepper-style `−` / value / `+` buttons (larger hit targets than `Stepper`), 0.5° or 1° per the device's step.
- Mode / fan speed: `Picker` with `.segmented` style when ≤ 4 options, otherwise `.menu`.
- Volume: `Slider` plus mute button (`speaker.slash.fill` / `speaker.wave.2.fill`).
- TV navigation: D-pad from SF Symbols (`chevron.up`, …) around an OK button; Back and Home below.
- Menu bar icon: a monochrome template SF Symbol (e.g. `switch.2` or `house`), never color.

## States every screen must define
loading · not configured (first run) · device offline/unreachable · auth expired (ThinQ token) · pairing required (TV prompt) · command pending · error · normal.

## Accessibility
- Every icon-only button has an accessibility label.
- Full keyboard operability inside the popover; Esc closes it.
- Works in light and dark mode and with Increase Contrast; no meaning conveyed by color alone.

## Design spec template (docs/design/<screen>.md)
```markdown
# <Screen name>
- status: draft | approved
- tasks: T5.4
## Goal
## Primary actions (ranked)
## Wireframe
(ASCII, with point sizes)
## Components
| Element | SwiftUI control | Tokens | Accessibility label |
## States
| State | What the user sees | What actions are available |
## Interactions & shortcuts
## Acceptance criteria (checkable from a screenshot)
- [ ] ...
```
