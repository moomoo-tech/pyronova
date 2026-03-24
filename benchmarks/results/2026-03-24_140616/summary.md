# Benchmark Summary

Generated: 2026-03-24T14:27:24.483441


## Basic Throughput

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Hello World | 216,297 (0.9ms) | 80,783 (4.1ms) | 201,161 (1.2ms) | 86,140 (25.9ms) |
| JSON small (3 fields) | 214,185 (0.9ms) | 76,034 (4.5ms) | 209,871 (1.1ms) | 82,725 (23.4ms) |
| JSON medium (100 users) | 67,376 (3.8ms) | 13,237 (22.1ms) | 64,755 (4.0ms) | 39,638 (26.7ms) |
| JSON large (500 records) | 5,045 (50.5ms) | 1,957 (157.5ms) | 4,899 (51.9ms) | 4,671 (85.1ms) |

## CPU Intensive

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| fib(10) | 187,340 (1.4ms) | 63,290 (5.0ms) | 202,607 (1.1ms) | 78,821 (24.1ms) |
| fib(20) | 10,051 (25.9ms) | 1,890 (164.3ms) | 10,988 (23.3ms) | 10,097 (42.3ms) |
| fib(30) | 90 (1250.0ms) | 1 (0.0ms) | 92 (1220.0ms) | 62 (267.1ms) |

## Python Ecosystem

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Pure Python sum(10k) | 74,546 (3.4ms) | 17,934 (16.3ms) | 77,016 (3.4ms) | 41,912 (18.1ms) |
| numpy mean(10k) | 8,484 (30.0ms) | 8,505 (30.1ms) | 32,932 (25.4ms) |
| numpy SVD 100x100 | 3,759 (67.8ms) | 3,988 (63.9ms) | 5,002 (117.1ms) |

## I/O Simulation

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| sleep(1ms) | 7,892 (32.3ms) | 53,342 (5.0ms) | 7,874 (32.3ms) | 86,826 (4.3ms) |

## JSON Parsing

| Scenario | pyre_subinterp | pyre_gil | pyre_hybrid | robyn |
|---|---|---|---|---|
| Parse 41B JSON | 211,280 (1.0ms) | 65,002 (5.6ms) | 213,898 (0.9ms) | 77,777 (25.9ms) |
| Parse 7KB JSON | 96,400 (2.6ms) | 20,012 (14.4ms) | 93,227 (2.9ms) | 46,980 (15.9ms) |
| Parse 93KB JSON | 9,828 (25.9ms) | 1,887 (164.7ms) | 8,993 (28.5ms) | 8,140 (45.9ms) |