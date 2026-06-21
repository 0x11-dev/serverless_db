import { type Actor, actorClaim } from "./auth.js";

export type PolicyRule =
  | { allow: boolean }
  | { role_in: string[] }
  | { column: string; equals_claim: string }
  | { column: string; in_claim: string }
  | { column: string; equals: SqlValue }
  | { and: PolicyRule[] }
  | { or: PolicyRule[] };

export type SqlValue = string | number | null;

export class PolicyError extends Error {}

const IDENT_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

export function quoteIdent(name: string): string {
  if (!IDENT_RE.test(name)) {
    throw new PolicyError(`invalid identifier: ${name}`);
  }
  return `"${name}"`;
}

export function compilePolicies(rules: PolicyRule[], actor: Actor): { sql: string; params: SqlValue[] } {
  if (actor.role === "service_role") {
    return { sql: "1=1", params: [] };
  }
  if (rules.length === 0) {
    return { sql: "1=1", params: [] };
  }
  const compiled = rules.map((rule) => compileRule(rule, actor));
  return join("OR", compiled);
}

export function evaluatePolicies(rules: PolicyRule[], row: Record<string, unknown>, actor: Actor): boolean {
  if (actor.role === "service_role") {
    return true;
  }
  if (rules.length === 0) {
    return true;
  }
  return rules.some((rule) => evaluateRule(rule, row, actor));
}

function compileRule(rule: PolicyRule, actor: Actor): { sql: string; params: SqlValue[] } {
  if ("allow" in rule) {
    return { sql: rule.allow ? "1=1" : "0=1", params: [] };
  }
  if ("role_in" in rule) {
    return { sql: rule.role_in.includes(actor.role) ? "1=1" : "0=1", params: [] };
  }
  if ("and" in rule) {
    return join("AND", rule.and.map((child) => compileRule(child, actor)));
  }
  if ("or" in rule) {
    return join("OR", rule.or.map((child) => compileRule(child, actor)));
  }
  if ("equals_claim" in rule) {
    const value = actorClaim(actor, rule.equals_claim);
    if (!isSqlValue(value)) {
      return { sql: "0=1", params: [] };
    }
    return { sql: `${quoteIdent(rule.column)} = ?`, params: [value] };
  }
  if ("in_claim" in rule) {
    const value = actorClaim(actor, rule.in_claim);
    if (!Array.isArray(value)) {
      return { sql: "0=1", params: [] };
    }
    const values = value.filter(isSqlValue);
    if (values.length === 0) {
      return { sql: "0=1", params: [] };
    }
    return { sql: `${quoteIdent(rule.column)} IN (${values.map(() => "?").join(",")})`, params: values };
  }
  if ("equals" in rule) {
    return { sql: `${quoteIdent(rule.column)} = ?`, params: [rule.equals] };
  }
  throw new PolicyError("unsupported policy rule");
}

function evaluateRule(rule: PolicyRule, row: Record<string, unknown>, actor: Actor): boolean {
  if ("allow" in rule) return rule.allow;
  if ("role_in" in rule) return rule.role_in.includes(actor.role);
  if ("and" in rule) return rule.and.every((child) => evaluateRule(child, row, actor));
  if ("or" in rule) return rule.or.some((child) => evaluateRule(child, row, actor));
  if ("equals_claim" in rule) return row[rule.column] === actorClaim(actor, rule.equals_claim);
  if ("in_claim" in rule) {
    const value = actorClaim(actor, rule.in_claim);
    return Array.isArray(value) && value.includes(row[rule.column]);
  }
  if ("equals" in rule) return row[rule.column] === rule.equals;
  return false;
}

function join(operator: "AND" | "OR", compiled: Array<{ sql: string; params: SqlValue[] }>): { sql: string; params: SqlValue[] } {
  if (compiled.length === 0) {
    throw new PolicyError("compound policy must not be empty");
  }
  return {
    sql: compiled.map((item) => `(${item.sql})`).join(` ${operator} `),
    params: compiled.flatMap((item) => item.params)
  };
}

function isSqlValue(value: unknown): value is SqlValue {
  return typeof value === "string" || typeof value === "number" || value === null;
}

