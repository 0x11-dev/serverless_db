import {
  appendFileSync,
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
  writeSync
} from "node:fs";
import { fsyncSync } from "node:fs";
import path from "node:path";

export class LocalObjectStore {
  readonly baseDir: string;

  constructor(baseDir: string) {
    this.baseDir = path.resolve(baseDir);
    mkdirSync(this.baseDir, { recursive: true });
  }

  path(...parts: string[]): string {
    const target = path.resolve(this.baseDir, ...parts);
    if (target !== this.baseDir && !target.startsWith(`${this.baseDir}${path.sep}`)) {
      throw new Error("object store path escapes base directory");
    }
    return target;
  }

  exists(...parts: string[]): boolean {
    return existsSync(this.path(...parts));
  }

  stat(...parts: string[]) {
    return statSync(this.path(...parts));
  }

  readBytes(...parts: string[]): Buffer {
    return readFileSync(this.path(...parts));
  }

  writeBytesAtomic(data: Buffer, ...parts: string[]): string {
    const dest = this.path(...parts);
    mkdirSync(path.dirname(dest), { recursive: true });
    const tmp = path.join(path.dirname(dest), `.${path.basename(dest)}.tmp-${process.pid}`);
    writeFileSync(tmp, data);
    renameSync(tmp, dest);
    return dest;
  }

  replaceFileAtomic(source: string, ...parts: string[]): string {
    const dest = this.path(...parts);
    mkdirSync(path.dirname(dest), { recursive: true });
    const tmp = path.join(path.dirname(dest), `.${path.basename(dest)}.tmp-${process.pid}`);
    copyFileSync(source, tmp);
    renameSync(tmp, dest);
    return dest;
  }

  appendFileRange(source: string, offset: number, ...parts: string[]): string {
    const dest = this.path(...parts);
    mkdirSync(path.dirname(dest), { recursive: true });
    const sourceFd = openSync(source, "r");
    const destFd = openSync(dest, "a");
    try {
      const buffer = Buffer.allocUnsafe(1024 * 1024);
      let position = offset;
      while (true) {
        const bytesRead = readSync(sourceFd, buffer, 0, buffer.length, position);
        if (bytesRead === 0) break;
        writeSync(destFd, buffer, 0, bytesRead);
        position += bytesRead;
      }
      fsyncSync(destFd);
    } finally {
      closeSync(sourceFd);
      closeSync(destFd);
    }
    return dest;
  }

  appendJsonl(record: Record<string, unknown>, ...parts: string[]): string {
    const dest = this.path(...parts);
    mkdirSync(path.dirname(dest), { recursive: true });
    appendFileSync(dest, `${JSON.stringify(record)}\n`, "utf8");
    const fd = openSync(dest, "r");
    try {
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    return dest;
  }

  remove(...parts: string[]): void {
    const target = this.path(...parts);
    if (existsSync(target)) {
      rmSync(target, { recursive: true, force: true });
    }
  }
}
