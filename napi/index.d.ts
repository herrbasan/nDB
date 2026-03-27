// Type definitions for ndb
// Human-readable document database for the AI age
// Part of the nGDB platform ecosystem.
//
// NOTE: This file is for LLM/editor context only. Not used at runtime.
// Vanilla JavaScript at runtime. No TypeScript.

export interface DatabaseOptions {
  persistence?: "lazy" | "immediate" | "scheduled";
  interval?: number;
}

export interface QueryOptions {
  limit?: number;
  offset?: number;
  sortBy?: string;
  sortDir?: "asc" | "desc";
}

export interface FileMeta {
  bucket: string;
  id: string;
  ext: string;
  name: string;
  size: number;
  type: string;
  created: number;
}

export declare class Database {
  constructor(path: string);
  static open(path: string, options?: DatabaseOptions): Database;
  static openInMemory(): Database;

  // Layer 1: Core Operations
  insert(doc: Record<string, any>): string;
  insertWithPrefix(prefix: string, doc: Record<string, any>): string;
  get(id: string): Record<string, any>;
  update(id: string, doc: Record<string, any>): void;
  delete(id: string): void;

  // Iteration & Counting
  iter(): Record<string, any>[];
  len(): number;
  isEmpty(): boolean;
  contains(id: string): boolean;

  // Layer 2: Single Field Queries
  find(field: string, value: any): Record<string, any>[];
  findRange(field: string, min: any, max: any): Record<string, any>[];

  // Layer 3: JSON AST Queries
  query(ast: Record<string, any>): Record<string, any>[];
  queryWith(
    ast: Record<string, any>,
    options?: QueryOptions
  ): Record<string, any>[];

  // Index Management
  createIndex(field: string): void;
  createBTreeIndex(field: string): void;
  dropIndex(field: string): void;
  hasIndex(field: string): boolean;

  // Compaction & Trash
  compact(): void;
  flush(): void;
  restore(id: string): void;
  deletedIds(): string[];

  // File Buckets
  storeFile(bucket: string, name: string, data: Buffer, mimeType: string): FileMeta;
  getFile(bucket: string, hash: string, ext: string): Buffer;
  deleteFile(bucket: string, hash: string, ext: string): void;
  listFiles(bucket: string): string[];
}
