# Use the official Rust image which has all compilation tools built-in
FROM rust:latest

# Force install GCC and standard C libraries just to be 10,000% certain
RUN apt-get update && apt-get install -y gcc libc6-dev

# Set the working directory
WORKDIR /app

# Copy your backend code into the container
COPY . .

# Build the Rust application
RUN cargo build --release

# Run the application
CMD ["cargo", "run", "--release"]