## write to a file named output.txt

if __name__ == "__main__":
    print("This is a shell reaction example. Please run the main.rs file in this directory to see it in action.")

    ## sleep for a random time between 1 and 5 seconds to simulate work
    import time
    import random
    sleep_time = random.uniform(1, 5)
    print(f"Sleeping for {sleep_time:.2f} seconds to simulate work...")
    time.sleep(sleep_time)
    ## get stdin data and print it
    import sys
    data = sys.stdin.read()
    ## print environment variables
    import os
    env_vars = os.environ
    
    ## exit -1 with low probability to simulate non-zero exit status
    import random
    if random.random() < 0.1:
        print("Simulating non-zero exit status")
        exit(-1)
    
    ## print all those data to a file
    with open("output.txt", "a") as f:
        f.write(f"Received data: {data}\n")
        ## write SENSOR_ID env var if it exists
        if "SENSOR_ID" in env_vars:
            f.write(f"SENSOR_ID: {env_vars['SENSOR_ID']}\n")
        ## write EXAMPLE_ENV env var if it exists
        if "EXAMPLE_ENV" in env_vars:
            f.write(f"EXAMPLE_ENV: {env_vars['EXAMPLE_ENV']}\n")

    print("Data and environment variables have been written to output.txt")