import argparse
import csv
import re
import plotly.graph_objects as go
from plotly.subplots import make_subplots

parser = argparse.ArgumentParser()
parser.add_argument("file", help="Input file with queue levels")
parser.add_argument("--include-filter", help="Regular expression for queue names that should be included")
parser.add_argument("--exclude-filter", help="Regular expression for queue names that should be excluded")
parser.add_argument("--bytes", help="include bytes levels", action="store_true")
parser.add_argument("--time", help="include time levels (default if none of the others are enabled)", action="store_true")
parser.add_argument("--buffers", help="include buffers levels", action="store_true")
parser.add_argument("--no-max", help="do not include max levels (enabled by default)", action="store_true")
parser.add_argument("--line-mode", help="show current levels with lines instead of points", action="store_true")
parser.add_argument("--focus-queue", help="focus on a specific queue name, hiding all others")
args = parser.parse_args()

include_filter = None
if args.include_filter is not None:
    include_filter = re.compile(args.include_filter)
exclude_filter = None
if args.exclude_filter is not None:
    exclude_filter = re.compile(args.exclude_filter)

queues = {}

with open(args.file, mode='r', encoding='utf_8', newline='') as csvfile:
    reader = csv.reader(csvfile, delimiter=',', quotechar='|')
    for row in reader:
        if len(row) != 9:
            continue

        if include_filter is not None and not include_filter.match(row[1]):
            continue
        if exclude_filter is not None and exclude_filter.match(row[1]):
            continue

        if not row[1] in queues:
            queues[row[1]] = {
                'cur-level-bytes': [],
                'cur-level-time': [],
                'cur-level-buffers': [],
                'max-size-bytes': [],
                'max-size-time': [],
                'max-size-buffers': [],
                'max-bytes-value': 0,  # Track maximum bytes value
            }

        wallclock = float(row[0]) / 1000000000.0
        bytes_value = int(row[3])
        queues[row[1]]['cur-level-bytes'].append((wallclock, bytes_value))
        queues[row[1]]['cur-level-time'].append((wallclock, float(row[4]) / 1000000000.0))
        queues[row[1]]['cur-level-buffers'].append((wallclock, int(row[5])))
        queues[row[1]]['max-size-bytes'].append((wallclock, int(row[6])))
        queues[row[1]]['max-size-time'].append((wallclock, float(row[7]) / 1000000000.0))
        queues[row[1]]['max-size-buffers'].append((wallclock, int(row[8])))

        # Update maximum bytes value seen for this queue
        if bytes_value > queues[row[1]]['max-bytes-value']:
            queues[row[1]]['max-bytes-value'] = bytes_value

# Determine which plots to create
plots = []
if args.buffers:
    plots.append(("buffers", "buffers"))
if args.time:
    plots.append(("time", "time (s)"))
if args.bytes:
    plots.append(("bytes", "bytes"))

# Default to time if no plots specified
if not plots:
    plots.append(("time", "time (s)"))

# Create a fixed color map for queues to ensure consistency
queue_names = list(queues.keys())
colors = [
    '#1f77b4', '#ff7f0e', '#2ca02c', '#d62728', '#9467bd',
    '#8c564b', '#e377c2', '#7f7f7f', '#bcbd22', '#17becf'
]
# Ensure we have enough colors by cycling if needed
while len(colors) < len(queue_names):
    colors.extend(colors[:len(queue_names) - len(colors)])

# Create a mapping of queue names to specific colors
queue_colors = {queue_name: colors[i] for i, queue_name in enumerate(queue_names)}

# Create subplot figure
fig = make_subplots(
    rows=len(plots),
    cols=1,
    shared_xaxes=True,
    vertical_spacing=0.1,
    subplot_titles=[ylabel for _, ylabel in plots]
)

# Add traces to figure
for queue_name, values in queues.items():
    # Get the fixed color for this queue
    color = queue_colors[queue_name]

    for row, (plot_type, _) in enumerate(plots, start=1):
        if plot_type == "buffers":
            # Current level as markers or lines depending on line_mode flag
            current_mode = "lines+markers" if args.line_mode else "markers"
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values['cur-level-buffers']],
                    y=[x[1] for x in values['cur-level-buffers']],
                    mode=current_mode,
                    name=f'{queue_name}: cur-level-buffers',
                    marker=dict(color=color),
                    line=dict(color=color, width=1.5, dash='solid'),
                    legendgroup=queue_name,
                    showlegend=(row == 1)  # Only show in legend once
                ),
                row=row, col=1
            )

            # Max size as lines
            if not args.no_max:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in values['max-size-buffers']],
                        y=[x[1] for x in values['max-size-buffers']],
                        mode='lines',
                        name=f'{queue_name}: max-size-buffers',
                        line=dict(color=color, width=2, dash='dot'),
                        legendgroup=queue_name,
                        showlegend=False  # Don't show this in the legend
                    ),
                    row=row, col=1
                )

        elif plot_type == "time":
            # Current level as markers or lines depending on line_mode flag
            current_mode = "lines+markers" if args.line_mode else "markers"
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values['cur-level-time']],
                    y=[x[1] for x in values['cur-level-time']],
                    mode=current_mode,
                    name=f'{queue_name}: cur-level-time',
                    marker=dict(color=color),
                    line=dict(color=color, width=1.5, dash='solid'),
                    legendgroup=queue_name,
                    showlegend=(row == 1 and "buffers" not in [p[0] for p in plots])
                ),
                row=row, col=1
            )

            # Max size as lines
            if not args.no_max:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in values['max-size-time']],
                        y=[x[1] for x in values['max-size-time']],
                        mode='lines',
                        name=f'{queue_name}: max-size-time',
                        line=dict(color=color, width=2, dash='dot'),
                        legendgroup=queue_name,
                        showlegend=False
                    ),
                    row=row, col=1
                )

        elif plot_type == "bytes":
            # Current level as markers or lines depending on line_mode flag
            current_mode = "lines+markers" if args.line_mode else "markers"
            fig.add_trace(
                go.Scatter(
                    x=[x[0] for x in values['cur-level-bytes']],
                    y=[x[1] for x in values['cur-level-bytes']],
                    mode=current_mode,
                    name=f'{queue_name}: cur-level-bytes',
                    marker=dict(color=color),
                    line=dict(color=color, width=1.5, dash='solid'),
                    legendgroup=queue_name,
                    showlegend=(row == 1 and not any(p[0] in ["buffers", "time"] for p in plots[:row-1]))
                ),
                row=row, col=1
            )

            # Max size as lines
            if not args.no_max:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in values['max-size-bytes']],
                        y=[x[1] for x in values['max-size-bytes']],
                        mode='lines',
                        name=f'{queue_name}: max-size-bytes',
                        line=dict(color=color, width=2, dash='dot'),
                        legendgroup=queue_name,
                        showlegend=False
                    ),
                    row=row, col=1
                )

# Update layout
title_text = 'Queue Levels'
if args.focus_queue and args.focus_queue in queues:
    title_text = f'Queue Levels - Focus on: {args.focus_queue}'

# Add updatemenus for queue selection
updatemenus = [
    dict(
        buttons=[
            dict(
                args=[{'visible': [True] * len(fig.data)}],
                label="Show All Queues",
                method="update"
            )
        ],
        direction="down",
        pad={"r": 10, "t": 10},
        showactive=True,
        x=0.1,
        xanchor="left",
        y=1.1,
        yanchor="top",
        bgcolor='lightgray',
        bordercolor='gray',
        font=dict(size=12)
    )
]

# Add buttons for each queue
visible_queues = list(queues.keys())
for queue_name in visible_queues:
    # Create visibility list - True only for traces of this queue
    queue_visibility = []
    for data_item in fig.data:
        # Check if this trace belongs to the current queue
        if queue_name in data_item.name:
            queue_visibility.append(True)
        else:
            queue_visibility.append(False)

    # Add the button for this queue
    updatemenus[0]['buttons'].append(
        dict(
            args=[{'visible': queue_visibility}],
            label=f"Focus on: {queue_name}",
            method="update"
        )
    )

fig.update_layout(
    title=title_text,
    xaxis_title='wallclock (s)',
    legend_title='Queues',
    height=800,  # Fixed larger height
    width=1200,  # Fixed larger width
    autosize=True,  # Allow autosize
    margin=dict(l=50, r=50, t=80, b=50),  # Minimize margins
    legend=dict(
        groupclick="toggleitem"
    ),
    updatemenus=updatemenus
)

# Add a note about the line styles
if not args.no_max:
    fig.add_annotation(
        xref="paper", yref="paper",
        x=0.5, y=1.05,
        text="Solid lines/points: Current level | Dotted lines: Maximum size",
        showarrow=False,
        font=dict(size=12)
    )

# Update y-axis titles
for i, (_, ylabel) in enumerate(plots, start=1):
    fig.update_yaxes(title_text=ylabel, row=i, col=1)

# Show the figure
fig.show()
