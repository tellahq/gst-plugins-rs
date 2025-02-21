import argparse
import csv
import re
from collections import defaultdict

import plotly.graph_objects as go
from plotly.subplots import make_subplots

parser = argparse.ArgumentParser()
parser.add_argument("file", help="Input file with queue levels")
parser.add_argument(
    "--include-filter",
    help="Regular expression for queue names that should be included",
)
parser.add_argument(
    "--exclude-filter",
    help="Regular expression for queue names that should be excluded",
)
parser.add_argument("--bytes", help="include bytes levels", action="store_true")
parser.add_argument(
    "--time",
    help="include time levels (default if none of the others are enabled)",
    action="store_true",
)
parser.add_argument("--buffers", help="include buffers levels", action="store_true")
parser.add_argument(
    "--no-max",
    help="do not include max levels (enabled by default)",
    action="store_true",
)
args = parser.parse_args()

include_filter = None
if args.include_filter is not None:
    include_filter = re.compile(args.include_filter)
exclude_filter = None
if args.exclude_filter is not None:
    exclude_filter = re.compile(args.exclude_filter)

queues = {}
# Dictionary to track total memory at each timestamp
total_memory_by_timestamp = defaultdict(int)

with open(args.file, mode="r", encoding="utf_8", newline="") as csvfile:
    reader = csv.reader(csvfile, delimiter=",", quotechar="|")
    for row in reader:
        if len(row) != 9:
            continue

        if include_filter is not None and not include_filter.match(row[1]):
            continue
        if exclude_filter is not None and exclude_filter.match(row[1]):
            continue

        timestamp = float(row[0])  # nanoseconds
        current_bytes = int(row[3])

        # Add this queue's current memory to the total for this timestamp
        total_memory_by_timestamp[timestamp] += current_bytes

        if not row[1] in queues:
            queues[row[1]] = {
                "cur-level-bytes": [],
                "cur-level-time": [],
                "cur-level-buffers": [],
                "max-size-bytes": [],
                "max-size-time": [],
                "max-size-buffers": [],
                "max-bytes-seen": 0,
                "max-bytes-timestamp": 0,
            }

        wallclock = timestamp / 1000000000.0

        # Track maximum bytes and its timestamp for each queue
        if current_bytes > queues[row[1]]["max-bytes-seen"]:
            queues[row[1]]["max-bytes-seen"] = current_bytes
            queues[row[1]]["max-bytes-timestamp"] = timestamp

        queues[row[1]]["cur-level-bytes"].append(
            (wallclock, current_bytes / (1024 * 1024))
        )
        queues[row[1]]["cur-level-time"].append(
            (wallclock, float(row[4]) / 1000000000.0)
        )
        queues[row[1]]["cur-level-buffers"].append((wallclock, int(row[5])))
        queues[row[1]]["max-size-bytes"].append(
            (wallclock, int(row[6]) / (1024 * 1024))
        )
        queues[row[1]]["max-size-time"].append(
            (wallclock, float(row[7]) / 1000000000.0)
        )
        queues[row[1]]["max-size-buffers"].append((wallclock, int(row[8])))


# Find the timestamp with maximum total memory
max_total_timestamp = max(total_memory_by_timestamp.items(), key=lambda x: x[1])[0]
max(total_memory_by_timestamp.items(), key=lambda x: x[1])[0]
max_total_memory = total_memory_by_timestamp[max_total_timestamp]

# Print the maximum total memory information
print("\nTotal Memory Peak Information:")
print("-" * 80)
print(f"Maximum total memory: {max_total_memory / (1024 * 1024):,.2f} MB")
print(f"Occurred at: {max_total_timestamp / 1000000:,.2f} ms")
print("\nQueue contributions at peak time:")
print("-" * 80)
print(f"{'Queue Name':<40} {'Size (MB)':>15} {'% of Total':>15}")
print("-" * 80)

# Get individual queue sizes at the peak time
queue_sizes_at_peak = {}
for queue_name, data in queues.items():
    # Find the closest timestamp in the queue's data
    closest_data = min(
        data["cur-level-bytes"],
        key=lambda x: abs(x[0] - max_total_timestamp / 1000000000.0),
    )
    queue_sizes_at_peak[f"{queue_name}"] = closest_data[1]  # Already in MB

# Sort queues by their size at peak time
sorted_queues_at_peak = sorted(
    queue_sizes_at_peak.items(), key=lambda x: x[1], reverse=True
)

# Print each queue's contribution at peak time
for queue_name, size_mb in sorted_queues_at_peak:
    percentage = (size_mb * 1024 * 1024 / max_total_memory) * 100
    print(f"{queue_name:<40} {size_mb:>15,.2f} {percentage:>14.1f}%")

print("\nPer-Queue Maximum Values (over entire duration):")
print("-" * 80)
print(f"{'Queue Name':<40} {'Max Size (MB)':>15} {'Timestamp (ms)':>20}")
print("-" * 80)

# Sort queues by their maximum size over the entire duration
sorted_queues = sorted(
    [(name, data["max-bytes-seen"]) for name, data in queues.items()],
    key=lambda x: x[1],
    reverse=True,
)

for queue_name, max_bytes in sorted_queues:
    max_mb = max_bytes / (1024 * 1024)
    timestamp_ms = queues[queue_name]["max-bytes-timestamp"] / 1000000
    print(f"{queue_name:<40} {max_mb:>15,.2f} {timestamp_ms:>20,.2f}")

# Setup the plotting
num_plots = 0
subplot_titles = []
if args.buffers:
    num_plots += 1
    subplot_titles.append("Buffers")
if args.time:
    num_plots += 1
    subplot_titles.append("Time (s)")
if args.bytes:
    num_plots += 1
    subplot_titles.append("MB")

if num_plots == 0:
    num_plots += 1
    subplot_titles.append("Time (s)")

# Create subplots
fig = make_subplots(
    rows=num_plots, cols=1, shared_xaxes=True, subplot_titles=subplot_titles
)

# Define some colors for the traces
colors = [
    "#1f77b4",
    "#ff7f0e",
    "#2ca02c",
    "#d62728",
    "#9467bd",
    "#8c564b",
    "#e377c2",
    "#7f7f7f",
    "#bcbd22",
    "#17becf",
]

print('-------------PLTS------------')
for i, (queue, values) in enumerate(queues.items()):
    print(f'-------------> {i}')
    color = colors[i % len(colors)]
    row = 1

    if args.buffers:
        print(f"Scattering {queue}")
        # Current level buffers
        fig.add_trace(
            go.Scatter(
                x=[x[0] for x in values["cur-level-buffers"]],
                y=[x[1] for x in values["cur-level-buffers"]],
                name=f"{queue}: cur-level-buffers",
                mode="lines",
                marker=dict(color=color),
                legendgroup=queue,
                showlegend=True,
            ),
            row=row,
            col=1,
        )

        if not args.no_max:
            # Max size buffers
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values["max-size-buffers"]],
                    y=[x[1] for x in values["max-size-buffers"]],
                    name=f"{queue}: max-size-buffers",
                    mode="lines",
                    line=dict(color=color),
                    legendgroup=queue,
                    showlegend=True,
                ),
                row=row,
                col=1,
            )
        row += 1

    if args.time:
        # Current level time
        fig.add_trace(
            go.Scatter(
                x=[x[0] for x in values["cur-level-time"]],
                y=[x[1] for x in values["cur-level-time"]],
                name=f"{queue}: cur-level-time",
                mode="lines",
                marker=dict(color=color),
                legendgroup=queue,
                showlegend=True,
            ),
            row=row,
            col=1,
        )

        if not args.no_max:
            # Max size time
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values["max-size-time"]],
                    y=[x[1] for x in values["max-size-time"]],
                    name=f"{queue}: max-size-time",
                    mode="lines",
                    line=dict(color=color),
                    legendgroup=queue,
                    showlegend=True,
                ),
                row=row,
                col=1,
            )
        row += 1

    if args.bytes:
        # Current level bytes
        fig.add_trace(
            go.Scatter(
                x=[x[0] for x in values["cur-level-bytes"]],
                y=[x[1] for x in values["cur-level-bytes"]],
                name=f"{queue}: cur-level-bytes",
                mode="lines",
                marker=dict(color=color),
                legendgroup=queue,
                showlegend=True,
            ),
            row=row,
            col=1,
        )

        if not args.no_max:
            # Max size bytes
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values["max-size-bytes"]],
                    y=[x[1] for x in values["max-size-bytes"]],
                    name=f"{queue}: max-size-bytes",
                    mode="lines",
                    line=dict(color=color),
                    legendgroup=queue,
                    showlegend=True,
                ),
                row=row,
                col=1,
            )

# Update layout
fig.update_layout(
    height=600 * num_plots,
    showlegend=True,
    legend=dict(yanchor="middle", y=0.5, xanchor="right", x=1.15),
    xaxis_title="Wallclock (s)",
)

# Add grid to all subplots
for i in range(1, num_plots + 1):
    fig.update_xaxes(showgrid=True, gridwidth=1, gridcolor="LightGray", row=i, col=1)
    fig.update_yaxes(showgrid=True, gridwidth=1, gridcolor="LightGray", row=i, col=1)

# Show the plot
fig.show()
