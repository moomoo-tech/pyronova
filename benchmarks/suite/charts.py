#!/usr/bin/env python3
"""Generate benchmark visualization charts from raw.json results."""

import json
import sys
from pathlib import Path

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.ticker as ticker
except ImportError:
    print("pip install matplotlib to generate charts")
    sys.exit(1)


def load_results(results_dir: Path) -> dict:
    raw_path = results_dir / "raw.json"
    with open(raw_path) as f:
        return json.load(f)


def plot_throughput_comparison(results: list, output_dir: Path):
    """Grouped bar chart: throughput by scenario, colored by framework."""
    frameworks = sorted(set(r["framework"] for r in results))
    scenarios = []
    for r in results:
        if r["scenario_id"] not in scenarios:
            scenarios.append(r["scenario_id"])

    colors = {
        "pyre_subinterp": "#2196F3",
        "pyre_hybrid": "#4CAF50",
        "pyre_gil": "#FF9800",
        "robyn": "#F44336",
    }

    fig, ax = plt.subplots(figsize=(16, 8))
    n = len(frameworks)
    width = 0.8 / n
    x_base = range(len(scenarios))

    for i, fw in enumerate(frameworks):
        vals = []
        for sid in scenarios:
            match = [r for r in results if r["framework"] == fw and r["scenario_id"] == sid]
            vals.append(match[0]["high_concurrency"]["req_per_sec"] if match else 0)
        positions = [x + i * width - (n - 1) * width / 2 for x in x_base]
        ax.bar(positions, vals, width * 0.9, label=fw, color=colors.get(fw, "#999"))

    scenario_names = []
    for sid in scenarios:
        match = [r for r in results if r["scenario_id"] == sid]
        scenario_names.append(match[0]["scenario_name"] if match else sid)

    ax.set_xlabel("Scenario")
    ax.set_ylabel("Requests/sec")
    ax.set_title("Throughput Comparison (High Concurrency: wrk -t4 -c256 -d10s)")
    ax.set_xticks(list(x_base))
    ax.set_xticklabels(scenario_names, rotation=45, ha="right", fontsize=8)
    ax.legend()
    ax.yaxis.set_major_formatter(ticker.FuncFormatter(lambda x, p: f"{x:,.0f}"))
    ax.grid(axis="y", alpha=0.3)
    plt.tight_layout()
    plt.savefig(output_dir / "throughput.png", dpi=150)
    plt.close()
    print(f"  Chart: {output_dir / 'throughput.png'}")


def plot_latency_comparison(results: list, output_dir: Path):
    """Latency comparison bar chart."""
    frameworks = sorted(set(r["framework"] for r in results))
    scenarios = []
    for r in results:
        if r["scenario_id"] not in scenarios:
            scenarios.append(r["scenario_id"])

    colors = {
        "pyre_subinterp": "#2196F3",
        "pyre_hybrid": "#4CAF50",
        "pyre_gil": "#FF9800",
        "robyn": "#F44336",
    }

    fig, ax = plt.subplots(figsize=(16, 8))
    n = len(frameworks)
    width = 0.8 / n
    x_base = range(len(scenarios))

    for i, fw in enumerate(frameworks):
        vals = []
        for sid in scenarios:
            match = [r for r in results if r["framework"] == fw and r["scenario_id"] == sid]
            vals.append(match[0]["high_concurrency"]["avg_latency_ms"] if match else 0)
        positions = [x + i * width - (n - 1) * width / 2 for x in x_base]
        ax.bar(positions, vals, width * 0.9, label=fw, color=colors.get(fw, "#999"))

    scenario_names = []
    for sid in scenarios:
        match = [r for r in results if r["scenario_id"] == sid]
        scenario_names.append(match[0]["scenario_name"] if match else sid)

    ax.set_xlabel("Scenario")
    ax.set_ylabel("Avg Latency (ms)")
    ax.set_title("Latency Comparison (High Concurrency)")
    ax.set_xticks(list(x_base))
    ax.set_xticklabels(scenario_names, rotation=45, ha="right", fontsize=8)
    ax.legend()
    ax.grid(axis="y", alpha=0.3)
    plt.tight_layout()
    plt.savefig(output_dir / "latency.png", dpi=150)
    plt.close()
    print(f"  Chart: {output_dir / 'latency.png'}")


def plot_pyre_radar(results: list, output_dir: Path):
    """Pyre vs Robyn win/loss summary."""
    pyre_best = {}  # scenario → best pyre req/s
    robyn_vals = {}

    for r in results:
        sid = r["scenario_id"]
        rps = r["high_concurrency"]["req_per_sec"]
        if r["framework"].startswith("pyre"):
            if sid not in pyre_best or rps > pyre_best[sid]:
                pyre_best[sid] = rps
        elif r["framework"] == "robyn":
            robyn_vals[sid] = rps

    common = sorted(set(pyre_best) & set(robyn_vals))
    if not common:
        return

    ratios = [pyre_best[s] / max(robyn_vals[s], 1) for s in common]
    names = []
    for sid in common:
        match = [r for r in results if r["scenario_id"] == sid]
        names.append(match[0]["scenario_name"][:20] if match else sid)

    fig, ax = plt.subplots(figsize=(14, 6))
    colors_bar = ["#4CAF50" if r >= 1.0 else "#F44336" for r in ratios]
    bars = ax.barh(names, ratios, color=colors_bar)
    ax.axvline(x=1.0, color="black", linewidth=1, linestyle="--", label="Parity (1.0x)")
    ax.set_xlabel("Pyre / Robyn Ratio")
    ax.set_title("Pyre vs Robyn — Per-Scenario Speedup")

    for bar, ratio in zip(bars, ratios):
        ax.text(bar.get_width() + 0.05, bar.get_y() + bar.get_height() / 2,
                f"{ratio:.1f}x", va="center", fontsize=9)

    ax.legend()
    plt.tight_layout()
    plt.savefig(output_dir / "pyre_vs_robyn.png", dpi=150)
    plt.close()
    print(f"  Chart: {output_dir / 'pyre_vs_robyn.png'}")


def generate_charts(results_dir: str | Path):
    results_dir = Path(results_dir)
    charts_dir = results_dir / "charts"
    charts_dir.mkdir(exist_ok=True)

    data = load_results(results_dir)
    results = data["results"]

    print(f"\n  Generating charts in {charts_dir}/")
    plot_throughput_comparison(results, charts_dir)
    plot_latency_comparison(results, charts_dir)
    plot_pyre_radar(results, charts_dir)
    print(f"  Done! {len(list(charts_dir.glob('*.png')))} charts generated.\n")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        # Find latest results dir
        results_base = Path(__file__).parent.parent / "results"
        dirs = sorted(results_base.iterdir())
        if not dirs:
            print("No results found. Run runner.py first.")
            sys.exit(1)
        results_dir = dirs[-1]
    else:
        results_dir = Path(sys.argv[1])

    generate_charts(results_dir)
