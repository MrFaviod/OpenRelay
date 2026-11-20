# Running the tests

The Rust workspace uses the standard test runner:

```sh
cargo test --locked --workspace
```

The end-to-end test checks that a synthetic phone streams a tone and that the tone is recorded from the `openrelay-mic` source. It needs Docker and runs with:

```sh
scripts/e2e/run.sh
```

Android unit tests:

```sh
cd android
./gradlew test
```
