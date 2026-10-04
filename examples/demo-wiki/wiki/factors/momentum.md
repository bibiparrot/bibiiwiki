---
title: Momentum Factor
type: factor
status: active
tags: [demo, momentum]
sources: [wiki/sources/sample-note.md]
---

# Momentum Factor

Momentum compares recent price strength across assets. This page uses
fictional data to demonstrate how a factor can connect its definition,
calculation, and source evidence in one readable Markdown note.

## Definition

Rank assets by their trailing return over a chosen lookback window. A higher
rank represents stronger recent performance; it does not guarantee future
returns.

## Example calculation

| Asset | Prior price | Current price | Trailing return |
| --- | ---: | ---: | ---: |
| Example A | 100 | 112 | 12% |
| Example B | 100 | 105 | 5% |
| Example C | 100 | 98 | -2% |

## Research notes

1. Select a lookback window and a clearly defined asset universe.
2. Compute returns with a consistent price-adjustment policy.
3. Validate the factor out of sample before using it in a strategy.

See [Research Cycle](../methodology/research-cycle.md) for the broader workflow and
[Sample Source Note](../sources/sample-note.md) for this demonstration's evidence.
