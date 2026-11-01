// Tests of today's (R2.18 to R2.21) rendering turn developer mode on: with
// it on, every element renders the ids, commands and exact times it did.
import { afterEach, beforeEach } from "vitest";
import { setDevMode } from "../ui/dev-mode";

export function withDevMode() {
  beforeEach(() => {
    setDevMode(true);
  });
  afterEach(() => {
    setDevMode(false);
  });
}
