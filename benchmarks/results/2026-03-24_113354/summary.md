# Benchmark Summary

Generated: 2026-03-24T11:55:02.404711


## Basic Throughput

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Hello World | 219,210 (0.8ms) | 119,049 (2.2ms) | 217,610 (0.9ms) | 83,849 (26.0ms) |
| JSON small (3 fields) | 211,951 (0.9ms) | 114,271 (2.3ms) | 217,589 (0.9ms) | 82,737 (22.3ms) |
| JSON medium (100 users) | 59,943 (4.5ms) | 14,737 (17.7ms) | 66,114 (3.9ms) | 39,462 (17.8ms) |
| JSON large (500 records) | 4,739 (53.6ms) | 1,895 (134.2ms) | 4,812 (53.0ms) | 4,715 (72.5ms) |

## CPU Intensive

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| fib(10) | 199,758 (1.1ms) | 75,526 (3.5ms) | 199,003 (1.1ms) | 77,317 (28.1ms) |
| fib(20) | 10,477 (24.8ms) | 1,917 (132.7ms) | 10,367 (24.7ms) | 9,938 (34.3ms) |
| fib(30) | 90 (1260.0ms) | 12 (749.6ms) | 91 (1260.0ms) | 79 (1070.0ms) |

## Python Ecosystem

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Pure Python sum(10k) | 75,821 (3.4ms) | 18,632 (14.2ms) | 75,648 (3.6ms) | 38,074 (22.5ms) |
| numpy mean(10k) | 8,419 (31.0ms) | 8,671 (29.4ms) | 29,731 (29.3ms) |
| numpy SVD 100x100 | 4,083 (62.6ms) | 4,078 (62.6ms) | 5,062 (61.2ms) |

## I/O Simulation

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| sleep(1ms) | 7,894 (32.3ms) | 7,354 (35.2ms) | 7,894 (32.3ms) | 73,594 (4.7ms) |

## JSON Parsing

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Parse 41B JSON | 196,231 (1.2ms) | 102,164 (2.6ms) | 212,182 (0.9ms) | 67,946 (19.3ms) |
| Parse 7KB JSON | 92,672 (2.8ms) | 21,100 (13.6ms) | 95,061 (2.6ms) | 44,854 (16.8ms) |
| Parse 93KB JSON | 9,836 (25.9ms) | 1,976 (132.2ms) | 9,721 (26.3ms) | 7,849 (66.0ms) |