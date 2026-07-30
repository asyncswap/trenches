# THE CHART

A TradingView-grade candlestick chart, drawn in text. Press `c` (or `v`) on
either chain.

```
┌ ANSEM/SOL 1m candle [,] [.] ─────────────────[t] [c] [o] [l]┐
│                                    ▂▂▂                      │
│                              █ ▂▂▂ ███          0.002437 SOL│
│┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄█┄███┄███┄┄┄┄┄┄┄┄┄ sell        │
│                          ▄▄▄ █ ███ ███ ▄▄▄ ▄▄▄  0.002421 SOL│
│                      ███ ███ █                              │
│              ▄▄▄ ███ ███ ▀▀▀ ▀                              │
│┄┄┄┄▄▄▄┄▄▄▄┄▄▄███┄███┄▀▀▀┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄ buy         │
│▄▄▄ ███ ███ ██▀▀▀  │                             0.002395 SOL│
│▀▀▀ ▀▀▀ ▀▀▀ ▀▀     │                                         │
└──────────────────────────────────────────────────────────────┘
```

## The grammar

Every candle **opens where the previous one closed** — price draws as one
continuous line. Minutes nobody traded draw as a **flat line at the last
close**, extended live to *now*: five quiet minutes on a 1s chart read as five
minutes of flat, not a frozen screen. A wick that exists always gets at least
one pixel — a high is a fact.

Candles render at half-cell resolution (two price pixels per terminal row),
green up, red down, with a dotted rule across the chart at the live price.

## Your position, on the chart

Your own fills draw as horizontal dotted lines at the **price you paid** —
tagged `buy` (green) and `sell` (red) at the right edge. Entry, exit, and the
live price rule between them: the whole story of a position at a glance.

## Intervals

`,` and `.` walk the ladder: **1s · 5s · 15s · 1m · 5m · 10m · 15m · 1h ·
4h · 1d**.

Sub-minute charts read the live tape. Minute-and-up charts read a **sealed
candle store**: every trade the tape sees is folded once into 1-minute candles
and persisted per coin, up to a week deep. It survives restarts and grows as
you watch — which is what makes a real 4h or 1d chart possible in a terminal.
