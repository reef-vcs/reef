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
  { ignores: ["src/gen/", "node_modules/", "test-results/", "playwright-report/", "blob-report/"] },
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
  // The browser smoke test (#128): Node, and waits on the page's state, never
  // on time or on an idle network (the streams never let it go idle). The
  // list restates the base block's, since a later block's options replace it.
  {
    files: ["e2e/**", "playwright.config.ts"],
    languageOptions: { globals: { ...globals.node } },
    rules: {
      "@typescript-eslint/no-non-null-assertion": "off",
      // Playwright's fixtures: `async ({}, use) => ...` is its idiom, and its
      // `use` is not React's.
      "no-empty-pattern": "off",
      "react-hooks/rules-of-hooks": "off",
      "no-restricted-syntax": [
        "error",
        {
          selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
          message:
            "Never render API text as HTML: it comes from repositories, which may be hostile.",
        },
        {
          selector: "CallExpression[callee.property.name='waitForTimeout']",
          message: "Wait on the page's state with expect, never on time.",
        },
        {
          selector: "Literal[value='networkidle'], TemplateElement[value.raw='networkidle']",
          message: "The streams never let the network go idle: wait on the page's state.",
        },
      ],
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
