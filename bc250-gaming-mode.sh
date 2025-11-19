#!/bin/bash

CONTROL_FILE="/tmp/bc250-max-performance"

# Function to clean up the control file
cleanup() {
    rm -f "$CONTROL_FILE"
    echo "BC-250: Returning to Normal Mode"
}

# Set a trap to run cleanup function on script exit
trap cleanup EXIT

# Create the control file to signal max performance mode
touch "$CONTROL_FILE"
echo "BC-250: Max Performance Mode Activated"

# Run the game and wait for it to finish
# Pass all arguments to the game
"$@"
GAME_EXIT_CODE=$?

# The trap ensures cleanup runs even if the game crashes or is interrupted
# No explicit cleanup is needed here

# Exit with the same code as the game
exit $GAME_EXIT_CODE