import "../../src/main";

// The harness attaches to the normal app. The app never imports the harness.
void import("./nativeTokenizerBenchmark").then(
  ({ installNativeTokenizerBenchmarkSeam }) => {
    installNativeTokenizerBenchmarkSeam();
  },
);
