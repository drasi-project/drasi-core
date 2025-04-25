#!/bin/bash

# Define the output file
output_file="all_output_lto.txt"

# Clear the output file if it already exists
> "$output_file"

# Run each command and append its output to the output file
cargo run --release -- -s single_node_property_projection -e memory -r memory --seed 12345 >> "$output_file"
cargo run --release -- -s single_node_property_projection -e rocksdb -r rocksdb --seed 12345 >> "$output_file"
cargo run --release -- -s single_node_property_projection -e redis -r redis --seed 12345 >> "$output_file"

cargo run --release -- -s single_node_calculation_projection -e memory -r memory --seed 12345 >> "$output_file"
cargo run --release -- -s single_node_calculation_projection -e rocksdb -r rocksdb --seed 12345 >> "$output_file"
cargo run --release -- -s single_node_calculation_projection -e redis -r redis --seed 12345 >> "$output_file"

cargo run --release -- -s single_path_averaging_projection -e memory -r memory --seed 12345 >> "$output_file"
cargo run --release -- -s single_path_averaging_projection -e rocksdb -r rocksdb --seed 12345 >> "$output_file"
cargo run --release -- -s single_path_averaging_projection -e redis -r redis --seed 12345 >> "$output_file"

cargo run --release -- -s single_path_no_change_averaging_projection -e memory -r memory --seed 12345 >> "$output_file"
cargo run --release -- -s single_path_no_change_averaging_projection -e rocksdb -r rocksdb --seed 12345 >> "$output_file"
cargo run --release -- -s single_path_no_change_averaging_projection -e redis -r redis --seed 12345 >> "$output_file"
