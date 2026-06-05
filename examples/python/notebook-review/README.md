# Python Notebook Review Example

This notebook-style example runs a local review through the Python SDK preview.

Build the runner first:

```sh
cargo build --bin muzen-runner
export MUZEN_RUNNER_PATH="$PWD/target/debug/muzen-runner"
```

Then open `notebook_review.ipynb` from the repository root or from this
directory. The notebook locates `sdk/python` automatically for local checkout
usage.
