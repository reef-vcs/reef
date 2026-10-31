// Lint rules the scaffold holds every view to (R2.18): no timers that fetch,
// no HTML from the API, and the hooks rules.
import js from "@eslint/js";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

const noInterval = {
  name: "setInterval",
  message: "The UI never polls: subscribe to the operation log or presence instead.",
};
const noTimeout = {
  name: "setTimeout",
  message: "Only live/backoff.ts schedules work: use its `after` or `Backoff`.",
};

export default tseslint.config(
  { ignores: ["src/gen/", "node_modules/"] },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  {
    languageOptions: {
      globals: { ...globals.browser },
      parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname },
    },
  },
  reactHooks.configs.flat["recommended-latest"],
  {
    rules: {
      "no-restricted-globals": ["error", noInterval, noTimeout],
      "no-restricted-properties": [
        "error",
        ...["window", "globalThis", "self"].flatMap((object) => [
          { object, property: "setInterval", message: noInterval.message },
          { object, property: "setTimeout", message: noTimeout.message },
        ]),
      ],
      "no-restricted-syntax": [
        "error",
        {
          selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
          message:
            "Never render API text as HTML: it comes from repositories, which may be hostile.",
        },
      ],
      "@typescript-eslint/restrict-template-expressions": ["error", { allowNumber: true }],
    },
  },
  {
    files: ["src/live/backoff.ts"],
    rules: { "no-restricted-globals": ["error", noInterval] },
  },
  {
    files: ["**/*.test.ts", "**/*.test.tsx"],
    rules: {
      "@typescript-eslint/no-non-null-assertion": "off",
      "@typescript-eslint/require-await": "off",
    },
  },
  {
    files: ["scripts/**", "vite.config.ts", "eslint.config.js"],
    languageOptions: { globals: { ...globals.node } },
  },
  // Plain JavaScript has no types to check.
  {
    files: ["**/*.js", "**/*.mjs"],
    extends: [tseslint.configs.disableTypeChecked],
  },
);
