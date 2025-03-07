#!/usr/bin/env python3
import sys
import argparse
import numpy as np
import pandas as pd
import re
import csv
from collections import defaultdict
import plotly.graph_objects as go
from plotly.subplots import make_subplots


def parse_memory_data(csv_file):
    """Parse memory tracer data from CSV file."""
    # Read only required columns and use dtype specifications for better performance
    df = pd.read_csv(
        csv_file,
        names=["timestamp", "operation", "pointer", "parent", "type", "size"],
        dtype={
            "timestamp": np.int64,
            "operation": str,
            "pointer": str,
            "parent": str,
            "type": str,
            "size": np.float64,
        },
        usecols=["timestamp", "operation", "pointer", "parent", "type", "size"],
    )

    # Filter out entries with parents early
    df = df[df["parent"].apply(lambda x: int(x, 16) == 0)]

    # Convert size to MB once
    df["size"] = df["size"] / (1024 * 1024)

    # Sort by timestamp
    df = df.sort_values("timestamp")

    # Initialize data structures
    memory_by_type = defaultdict(lambda: defaultdict(float))
    memory_values_by_type = defaultdict(list)

    # Track active pooled buffers
    active_pooled_buffers = {}  # pointer -> (type, size)

    # Process in chunks for better memory usage
    chunk_size = 1
    total_in_pools = 0
    max_in_pools = 0
    max_in_pools_ts = 0

    for chunk_start in range(0, len(df), chunk_size):
        chunk = df.iloc[chunk_start : chunk_start + chunk_size]

        for _, row in chunk.iterrows():
            mem_type = row["type"]
            pointer = row["pointer"]
            timestamp = row["timestamp"] / 1000000000.0  # Convert to seconds
            size = row["size"]
            operation = row["operation"]

            # Handle regular memory operations (alloc/free)
            if operation == "alloc":
                memory_by_type[mem_type][pointer] = size
            elif operation == "free" and pointer in memory_by_type[mem_type]:
                del memory_by_type[mem_type][pointer]

            # Handle pool operations
            if operation == "queued":
                # Add to type-specific pooled category
                pooled_type = f'{mem_type}_pooled'

                # Store in active_pooled_buffers for tracking
                active_pooled_buffers[pointer] = (pooled_type, size)

                # Add to the pooled memory tracking
                memory_by_type[pooled_type][pointer] = size
                memory_by_type["TotalPooled"][pointer] = size

                total_in_pools += size
                if total_in_pools > max_in_pools:
                    max_in_pools = total_in_pools
                    max_in_pools_ts = timestamp

            elif operation == "dequeued":
                # Find the pointer in active_pooled_buffers
                if pointer in active_pooled_buffers:
                    pooled_type, _ = active_pooled_buffers[pointer]

                    # Remove from type-specific pool
                    if pointer in memory_by_type[pooled_type]:
                        del memory_by_type[pooled_type][pointer]

                    # Remove from TotalPooled
                    if pointer in memory_by_type["TotalPooled"]:
                        size_to_remove = memory_by_type["TotalPooled"][pointer]
                        del memory_by_type["TotalPooled"][pointer]
                        total_in_pools -= size_to_remove

                    # Remove from tracking
                    del active_pooled_buffers[pointer]

            # Record current state for all memory types
            # Regular memory types
            if operation in ["alloc", "free"]:
                current_memory = sum(memory_by_type[mem_type].values())
                buffer_count = len(memory_by_type[mem_type])

                extra_info = f"{operation}(0x{pointer}={size:.3f}MB)"

                memory_values_by_type[mem_type].append(
                    (timestamp, current_memory, buffer_count, extra_info)
                )

            # Handle recording of pooled memory types
            if operation in ["queued", "dequeued"]:
                pooled_type = f'{mem_type}_pooled'

                # Update metrics for the specific pooled type
                current_memory = sum(memory_by_type[pooled_type].values())
                buffer_count = len(memory_by_type[pooled_type])

                memory_values_by_type[pooled_type].append(
                    (timestamp, current_memory, buffer_count, operation)
                )

                # Update TotalPooled metrics
                total_memory = sum(memory_by_type["TotalPooled"].values())
                total_count = len(memory_by_type["TotalPooled"])

                memory_values_by_type["TotalPooled"].append(
                    (timestamp, total_memory, total_count, operation)
                )

    print(f"Maximum in pools: {max_in_pools} reached at {max_in_pools_ts}")

    return memory_values_by_type


def create_memory_graph(memory_values_by_type):
    """Create memory usage graph from parsed data."""
    fig = go.Figure()

    # Base colors for the different memory types
    colors = {
        "TotalPooled": "red",
        "SystemMemory": "blue",
        "GPU Memory": "darkblue",
        "GLBuffer": "orange",
        "GLMemoryPBO": "darkorange",
        "GLRenderBuffer": "darkturquoise",
        "gst.cuda.memory": "green",
        "DMABuf": "purple",
        "fd": "magenta",
        "shm": "darkviolet",
    }

    # Generate gradient colors for pooled buffers
    # First, identify all pooled buffer types
    pooled_types = [mem_type for mem_type in memory_values_by_type.keys() if '_pooled' in mem_type]

    # Create gradient colors from pink to dark red for pooled types
    red_gradients = {
        0: "#FFCCCC",  # Light pink
        1: "#FF9999",
        2: "#FF6666",
        3: "#FF3333",
        4: "#FF0000",  # Red
        5: "#CC0000",
        6: "#990000",
        7: "#660000",  # Dark red
    }

    # Assign colors to pooled types
    for i, pooled_type in enumerate(pooled_types):
        if pooled_type != "TotalPooled":  # TotalPooled already has a color
            colors[pooled_type] = red_gradients.get(i % len(red_gradients), "#AA0000")  # Default fallback

    # Add traces for each memory type
    for mem_type, values in memory_values_by_type.items():
        # Determine color - use the predefined color or assign one based on pattern
        if mem_type in colors:
            color = colors[mem_type]
        elif '_pooled' in mem_type:
            # If this is a pooled type we missed somehow, use a red shade
            color = "#BB0000"
        else:
            # Default to gray for unknown types
            color = "gray"

        decimation_factor = max(1, len(values) // 10000)

        # Decimate the data points
        decimated_values = values[::decimation_factor]
        decimated_timestamps = [v[0] for v in decimated_values]
        memory_values = [v[1] for v in decimated_values]  # Extract memory values
        buffer_counts = [v[2] for v in decimated_values]  # Extract buffer counts

        # Optimize hover text - only create it for visible points
        hover_text = [
            f"{mem_type}:<br>• Size: {mem:.2f}MB<br>• Buffers: {count}"
            for mem, count in zip(memory_values, buffer_counts)
        ]

        fig.add_trace(
            go.Scatter(
                x=decimated_timestamps,
                y=memory_values,
                mode="lines",  # Removed markers for better performance
                name=f"{mem_type}",
                line=dict(color=color, width=2),
                hovertext=hover_text,
                hoverinfo="text",
                hovertemplate="%{hovertext}<extra></extra>",  # Optimized hover template
            )
        )

    # Customize the layout
    fig.update_layout(
        title="GStreamer Memory Usage Over Time",
        xaxis_title="Timestamp (s)",
        yaxis_title="Memory Usage (MB)",
        hovermode="closest",  # Enable vertical line on hover
        showlegend=True,
        template="plotly_white",
        yaxis=dict(rangemode="nonnegative"),  # Ensure y-axis doesn't go below 0
        hoverlabel=dict(
            bgcolor="white",
            font_size=12,
            font_family="Arial",
            bordercolor="darkgray",
            namelength=-1  # Show the full trace name
        ),
    )

    return fig


def parse_queue_data(queue_file, include_filter=None, exclude_filter=None):
    """Parse queue level data from CSV file."""
    queues = {}

    with open(queue_file, mode='r', encoding='utf_8', newline='') as csvfile:
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

    return queues


def compute_max_across_queues(queues, metric_key):
    """
    Compute maximum values across all queues by tracking the last known value
    for each queue at any given time, only considering active (non-zero) queues.
    """
    # Create time-ordered list of all data points with queue information
    all_data_points = []

    for queue_name, values in queues.items():
        data_points = values[metric_key]
        for wallclock, value in data_points:
            all_data_points.append((wallclock, queue_name, value))

    # Sort by wallclock time
    all_data_points.sort()

    # Track the last known value for each queue
    last_known_values = {}
    # Track the total sum at each time point
    time_sums = []
    # Track the maximum sum and when it occurred
    max_sum = 0
    max_sum_time = 0
    max_sum_queue_values = {}

    # Process data points in chronological order
    for wallclock, queue_name, value in all_data_points:
        # Update the last known value for this queue - only track non-zero values
        if value > 0:
            last_known_values[queue_name] = value
        elif queue_name in last_known_values:
            # If a queue reports 0, remove it from active tracking
            del last_known_values[queue_name]

        # Calculate the current total across all active queues
        current_total = sum(last_known_values.values())

        # Add to our time series
        time_sums.append((wallclock, current_total))

        # Check if this is a new maximum
        if current_total > max_sum:
            max_sum = current_total
            max_sum_time = wallclock
            # Take a snapshot of all queue values at this time
            max_sum_queue_values = last_known_values.copy()

    # Convert max_sum_queue_values to the sorted format expected by the rest of the code
    sorted_queue_details = sorted(
        [(queue, value) for queue, value in max_sum_queue_values.items()],
        key=lambda x: x[1],
        reverse=True
    )

    return time_sums, (max_sum_time, max_sum), sorted_queue_details


def create_queue_graphs(queues, show_buffers=False, show_time=False, show_bytes=False,
                       no_max=False, line_mode=False, focus_queue=None, show_total=False):
    """Create queue level graphs from parsed data."""
    # Determine which plots to create
    plots = []
    if show_buffers:
        plots.append(("buffers", "buffers"))
    if show_time:
        plots.append(("time", "time (s)"))
    if show_bytes:
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
        # Skip this queue if we're focusing on a specific one and this isn't it
        if focus_queue and queue_name != focus_queue:
            continue

        # Get the fixed color for this queue
        color = queue_colors[queue_name]

        for row, (plot_type, _) in enumerate(plots, start=1):
            if plot_type == "buffers":
                # Current level as markers or lines depending on line_mode flag
                current_mode = "lines+markers" if line_mode else "markers"
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
                if not no_max:
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
                current_mode = "lines+markers" if line_mode else "markers"
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
                if not no_max:
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
                current_mode = "lines+markers" if line_mode else "markers"
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
                if not no_max:
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

    # Process total queue levels if requested
    if show_total:
        # Compute max across queues for each metric
        bytes_sums = []
        time_sums = []
        buffer_sums = []
        bytes_details = []
        time_details = []
        buffer_details = []

        if "bytes" in [p[0] for p in plots] or not plots:
            bytes_sums, max_bytes, bytes_details = compute_max_across_queues(queues, 'cur-level-bytes')

        if "time" in [p[0] for p in plots] or not plots:
            time_sums, max_time, time_details = compute_max_across_queues(queues, 'cur-level-time')

        if "buffers" in [p[0] for p in plots]:
            buffer_sums, max_buffers, buffer_details = compute_max_across_queues(queues, 'cur-level-buffers')

        # Add total to plots if requested
        for row, (plot_type, _) in enumerate(plots, start=1):
            if plot_type == "bytes" and bytes_sums:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in bytes_sums],
                        y=[x[1] for x in bytes_sums],
                        mode="lines",
                        name="Total bytes across all queues",
                        line=dict(color="red", width=1.0, dash="solid"),
                        legendgroup="total",
                        showlegend=True
                    ),
                    row=row, col=1
                )

                # Add annotation for maximum point
                fig.add_annotation(
                    x=max_bytes[0],
                    y=max_bytes[1],
                    text=f"Max: {max_bytes[1] / (1024 * 1024):.2f} MB",
                    showarrow=True,
                    arrowhead=1,
                    row=row, col=1
                )

            elif plot_type == "time" and time_sums:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in time_sums],
                        y=[x[1] for x in time_sums],
                        mode="lines",
                        name="Total time across all queues",
                        line=dict(color="red", width=1.0, dash="solid"),
                        legendgroup="total",
                        showlegend=True
                    ),
                    row=row, col=1
                )

                # Add annotation for maximum point
                fig.add_annotation(
                    x=max_time[0],
                    y=max_time[1],
                    text=f"Max: {max_time[1]:.6f}s",
                    showarrow=True,
                    arrowhead=1,
                    row=row, col=1
                )

            elif plot_type == "buffers" and buffer_sums:
                fig.add_trace(
                    go.Scatter(
                        x=[x[0] for x in buffer_sums],
                        y=[x[1] for x in buffer_sums],
                        mode="lines",
                        name="Total buffers across all queues",
                        line=dict(color="red", width=1.0, dash="solid"),
                        legendgroup="total",
                        showlegend=True
                    ),
                    row=row, col=1
                )

                # Add annotation for maximum point
                fig.add_annotation(
                    x=max_buffers[0],
                    y=max_buffers[1],
                    text=f"Max: {max_buffers[1]} buffers",
                    showarrow=True,
                    arrowhead=1,
                    row=row, col=1
                )

    # Update layout
    title_text = 'Queue Levels'
    if focus_queue and focus_queue in queues:
        title_text = f'Queue Levels - Focus on: {focus_queue}'
    if show_total:
        title_text += ' (with Total Across All Queues)'

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
            # Keep totals visible if they exist
            elif "Total" in data_item.name:
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

    # Add button to show only totals if totals are enabled
    if show_total:
        total_only_visibility = []
        for data_item in fig.data:
            if "Total" in data_item.name:
                total_only_visibility.append(True)
            else:
                total_only_visibility.append(False)

        updatemenus[0]['buttons'].append(
            dict(
                args=[{'visible': total_only_visibility}],
                label=f"Show only totals",
                method="update"
            )
        )

    fig.update_layout(
        title=title_text,
        xaxis_title='wallclock (s)',
        legend_title='Queues',
        height=800,  # Fixed larger height
        autosize=True,  # Allow autosize
        margin=dict(l=50, r=50, t=80, b=50),  # Minimize margins
        legend=dict(
            groupclick="toggleitem"
        ),
        updatemenus=updatemenus,
        hovermode="closest"  # Enable vertical line on hover
    )

    # Add a note about the line styles
    note_text = "Solid lines/points: Current level"
    if not no_max:
        note_text += " | Dotted lines: Maximum size"
    if show_total:
        note_text += " | Red line: Total across all queues"

    fig.add_annotation(
        xref="paper", yref="paper",
        x=0.5, y=1.05,
        text=note_text,
        showarrow=False,
        font=dict(size=12)
    )

    # Update y-axis titles
    for i, (_, ylabel) in enumerate(plots, start=1):
        fig.update_yaxes(title_text=ylabel, row=i, col=1)

    return fig


def create_combined_dashboard(memory_data, queue_data, plot_options):
    """Create a combined dashboard with memory and queue graphs."""
    # Create figures for the subplots
    memory_fig = create_memory_graph(memory_data)

    queue_fig = create_queue_graphs(
        queue_data,
        show_buffers=plot_options.buffers,
        show_time=plot_options.time,
        show_bytes=plot_options.bytes,
        no_max=plot_options.no_max,
        line_mode=plot_options.line_mode,
        focus_queue=plot_options.focus_queue,
        show_total=plot_options.show_total
    )

    # Determine number of queue plots
    queue_plots = []
    if plot_options.buffers:
        queue_plots.append("buffers")
    if plot_options.bytes:
        queue_plots.append("bytes")
    if plot_options.time or not queue_plots:  # Default to time
        queue_plots.append("time")

    # Create the combined figure
    total_rows = 1 + len(queue_plots)  # Memory plot + queue plots
    fig = make_subplots(
        rows=total_rows,
        cols=1,
        shared_xaxes=True,
        vertical_spacing=0.08,
        subplot_titles=["Memory Usage"] + queue_plots
    )

    # Add memory traces
    for trace in memory_fig.data:
        fig.add_trace(trace, row=1, col=1)

    # Add queue traces
    current_row = 2
    for plot_type in queue_plots:
        for trace in queue_fig.data:
            if plot_type in trace.name.lower():
                fig.add_trace(trace, row=current_row, col=1)
        current_row += 1

    # Update layout
    fig.update_layout(
        title="GStreamer Memory Usage and Queue Levels",
        height=300 * total_rows,  # Adjust height based on number of plots
        width=1200,
        showlegend=True,
        legend=dict(
            groupclick="toggleitem"
        ),
        margin=dict(l=50, r=50, t=100, b=50),
        hovermode="closest",  # This enables the vertical line across all subplots
    )

    # Update axis titles
    fig.update_yaxes(title_text="Memory (MB)", row=1, col=1)

    current_row = 2
    for plot_type in queue_plots:
        if plot_type == "buffers":
            fig.update_yaxes(title_text="Queue levels (Buffers)", row=current_row, col=1)
        elif plot_type == "time":
            fig.update_yaxes(title_text="Queues level (Time in secs)", row=current_row, col=1)
        elif plot_type == "bytes":
            fig.update_yaxes(title_text="Queues level (Bytes)", row=current_row, col=1)
        current_row += 1

    fig.update_xaxes(title_text="Timestamp (s)", row=total_rows, col=1)

    return fig


def main():
    parser = argparse.ArgumentParser(description="GStreamer Memory and Queue Analysis Tool")

    # Input files
    input_group = parser.add_argument_group('Input Files')
    input_group.add_argument("-m", "--memory-file", help="Input file with memory usage data")
    input_group.add_argument("-q", "--queue-file", help="Input file with queue levels data")

    # Queue analysis options
    queue_group = parser.add_argument_group('Queue Analysis Options')
    queue_group.add_argument("-i", "--include-filter", help="Regular expression for queue names that should be included")
    queue_group.add_argument("-e", "--exclude-filter", help="Regular expression for queue names that should be excluded")
    queue_group.add_argument("-b", "--bytes", help="Include bytes levels", action="store_true")
    queue_group.add_argument("-t", "--time", help="Include time levels (default if none of the others are enabled)", action="store_true")
    queue_group.add_argument("-u", "--buffers", help="Include buffers levels", action="store_true")
    queue_group.add_argument("-n", "--no-max", help="Do not include max levels (enabled by default)", action="store_true")
    queue_group.add_argument("-l", "--line-mode", help="Show current levels with lines instead of points", action="store_true")
    queue_group.add_argument("-f", "--focus-queue", help="Focus on a specific queue name, hiding all others")
    queue_group.add_argument("-s", "--show-total", help="Show the sum across all queues", action="store_true")

    args = parser.parse_args()

    # Check for at least one input file
    if not args.memory_file and not args.queue_file:
        parser.error("At least one input file (--memory-file or --queue-file) is required")

    # Process memory file if provided
    memory_data = None
    if args.memory_file:
        print(f"Processing memory trace data from {args.memory_file}...")
        memory_data = parse_memory_data(args.memory_file)

    # Process queue file if provided
    queue_data = None
    if args.queue_file:
        print(f"Processing queue levels data from {args.queue_file}...")
        include_filter = re.compile(args.include_filter) if args.include_filter else None
        exclude_filter = re.compile(args.exclude_filter) if args.exclude_filter else None
        queue_data = parse_queue_data(args.queue_file, include_filter, exclude_filter)

    # Generate appropriate visualization
    if memory_data and queue_data:
        print("Creating combined memory and queue visualization...")
        fig = create_combined_dashboard(memory_data, queue_data, args)
    elif memory_data:
        print("Creating memory usage visualization...")
        fig = create_memory_graph(memory_data)
    else:
        print("Creating queue levels visualization...")
        fig = create_queue_graphs(
            queue_data,
            show_buffers=args.buffers,
            show_time=args.time,
            show_bytes=args.bytes,
            no_max=args.no_max,
            line_mode=args.line_mode,
            focus_queue=args.focus_queue,
            show_total=args.show_total
        )

    # Display figure
    print("Opening visualization in browser...")
    fig.show()


if __name__ == "__main__":
    main()
