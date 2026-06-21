import { createHmac, timingSafeEqual } from "node:crypto";

export type Actor = {
  sub: string | null;
  role: string;
  claims: Record<string, unknown>;
};

export class AuthError extends Error {}

type TokenPayload = {
  sub?: string;
  role?: string;
  claims?: Record<string, unknown>;
  iat?: number;
  exp?: number;
};

const DEFAULT_SECRET = "dev-secret-change-me";

export function mintToken(input: {
  sub: string;
  role?: string;
  claims?: Record<string, unknown>;
  expiresIn?: number;
  secret?: string;
}): string {
  const now = Math.floor(Date.now() / 1000);
  const payload: TokenPayload = {
    sub: input.sub,
    role: input.role ?? "authenticated",
    claims: input.claims ?? {},
    iat: now
  };
  if (input.expiresIn !== undefined) {
    payload.exp = now + input.expiresIn;
  }
  const header = { alg: "HS256", typ: "JWT" };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(payload))}`;
  const sig = hmac(signingInput, input.secret);
  return `${signingInput}.${sig}`;
}

export function actorFromAuthorization(header: string | undefined): Actor {
  if (!header) {
    return { sub: null, role: "anon", claims: {} };
  }
  if (!header.startsWith("Bearer ")) {
    throw new AuthError("authorization header must use Bearer token");
  }
  return verifyToken(header.slice("Bearer ".length).trim());
}

export function verifyToken(token: string, secret?: string): Actor {
  const parts = token.split(".");
  if (parts.length !== 3) {
    throw new AuthError("invalid token format");
  }

  const signingInput = `${parts[0]}.${parts[1]}`;
  const expected = Buffer.from(hmac(signingInput, secret), "base64url");
  const actual = Buffer.from(parts[2], "base64url");
  if (expected.length !== actual.length || !timingSafeEqual(expected, actual)) {
    throw new AuthError("invalid token signature");
  }

  const header = JSON.parse(Buffer.from(parts[0], "base64url").toString("utf8")) as { alg?: string };
  if (header.alg !== "HS256") {
    throw new AuthError("unsupported token algorithm");
  }

  const payload = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8")) as TokenPayload;
  if (payload.exp !== undefined && payload.exp < Math.floor(Date.now() / 1000)) {
    throw new AuthError("token expired");
  }
  if (payload.claims !== undefined && !isRecord(payload.claims)) {
    throw new AuthError("token claims must be an object");
  }
  return {
    sub: payload.sub ?? null,
    role: payload.role ?? "authenticated",
    claims: payload.claims ?? {}
  };
}

export function actorClaim(actor: Actor, name: string): unknown {
  if (name === "sub") return actor.sub;
  if (name === "role") return actor.role;
  return actor.claims[name];
}

export function isAdmin(actor: Actor): boolean {
  return actor.role === "service_role" || actor.role === "admin";
}

function hmac(signingInput: string, secret?: string): string {
  return createHmac("sha256", secret ?? process.env.SDB_JWT_SECRET ?? DEFAULT_SECRET)
    .update(signingInput)
    .digest("base64url");
}

function base64url(value: string): string {
  return Buffer.from(value).toString("base64url");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
