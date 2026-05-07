import { defineConfig } from "@pandacss/dev";
import pandaPreset from "@pandacss/preset-panda";

export default defineConfig({
  preflight: true,
  include: ["./src/**/*.{js,jsx,ts,tsx}", "./pages/**/*.{js,jsx,ts,tsx}"],
  exclude: [],
  presets: [pandaPreset],
  theme: { extend: {} },
  outdir: "src/styled-system",
  jsxFramework: "solid",
});
