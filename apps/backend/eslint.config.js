import tseslint from "typescript-eslint";

export default tseslint.config(
  {
    ignores: [
      "worker-configuration.d.ts",
      "dist/**",
      ".wrangler/**",
      ".dev.vars*",
      "test/smoke-ws.mjs",
    ],
  },
  ...tseslint.configs.recommended,
  {
    rules: {
      "@typescript-eslint/no-unused-vars": [
        "error",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_", caughtErrors: "none" },
      ],
    },
  },
);
