/* tslint:disable */
/* eslint-disable */

/**
 * Database class for nDB
 *
 * Provides access to the embedded vector database. Each database
 * can contain multiple collections with different vector dimensions.
 */
export class Database {
  constructor(path: string);

  /**
   * Create a new collection with the specified dimension
   * @param name - Collection name
   * @param dimension - Vector dimension (e.g., 768, 1536)
   * @param options - Optional configuration
   * @returns The created Collection
   */
  createCollection(name: string, dimension: number, options?: CollectionOptions): Collection;

  /**
   * Get an existing collection
   * @param name - Collection name
   * @returns The Collection
   */
  getCollection(name: string): Collection;

  /**
   * List all collection names in the database
   * @returns Array of collection names
   */
  listCollections(): string[];
}

/**
 * Collection class for managing vectors
 */
export class Collection {
  /** Collection name */
  get name(): string;

  /** Collection configuration */
  get config(): CollectionConfig;

  /** Collection statistics */
  get stats(): CollectionStats;

  /**
   * Insert a single document
   * @param id - Unique document ID
   * @param vector - Embedding vector (array of numbers)
   * @param payload - Optional JSON payload as string
   */
  insert(id: string, vector: number[], payload?: string): void;

  /**
   * Insert multiple documents in a batch (more efficient)
   * @param docs - Array of documents to insert
   */
  insertBatch(docs: InsertDoc[]): void;

  /**
   * Get a document by ID
   * @param id - Document ID
   * @returns The document or null if not found
   */
  get(id: string): Document | null;

  /**
   * Delete a document by ID
   * @param id - Document ID
   * @returns true if document existed and was deleted
   */
  delete(id: string): boolean;

  /**
   * Search for similar vectors
   * @param options - Search configuration
   * @returns Array of matching documents with scores
   */
  search(options: SearchOptions): Match[];

  /** Flush memtable to disk (creates new segment) */
  flush(): void;

  /** Force WAL sync to disk for durability */
  sync(): void;

  /**
   * Compact segments to reclaim space
   * @returns Compaction statistics
   */
  compact(): CompactionResult;

  /** Build HNSW index for approximate search */
  rebuildIndex(): void;

  /** Delete HNSW index */
  deleteIndex(): void;

  /** Check if HNSW index exists */
  hasIndex(): boolean;
}

/** Collection configuration options */
export interface CollectionOptions {
  /** Durability level: 'buffered' (fast) or 'sync' (safe) */
  durability?: 'buffered' | 'sync';
}

/** Collection configuration (read-only) */
export interface CollectionConfig {
  /** Vector dimension */
  dim: number;
  /** Durability setting */
  durability: string;
}

/** Document for insertion */
export interface InsertDoc {
  id: string;
  vector: number[];
  payload?: string;
}

/** Document returned from get() */
export interface Document {
  id: string;
  vector: number[];
  payload?: string;
}

/** Search options */
export interface SearchOptions {
  /** Query vector */
  vector: number[];
  /** Number of results to return (default: 10) */
  topK?: number;
  /** Distance metric: 'cosine', 'dot', or 'euclidean' (default: 'cosine') */
  distance?: 'cosine' | 'dot' | 'euclidean';
  /** Use HNSW approximate search (default: false) */
  approximate?: boolean;
  /** HNSW quality parameter (default: use index default) */
  ef?: number;
  /** Filter JSON string (use FilterBuilder to construct) */
  filter?: string;
}

/** Search result match */
export interface Match {
  /** Document ID */
  id: string;
  /** Similarity score (higher is better for cosine/dot) */
  score: number;
  /** JSON payload if present */
  payload?: string;
}

/** Compaction result statistics */
export interface CompactionResult {
  /** Documents before compaction */
  docsBefore: number;
  /** Documents after compaction */
  docsAfter: number;
  /** Number of segments merged */
  segmentsMerged: number;
  /** Whether HNSW index was rebuilt */
  indexRebuilt: boolean;
}

/** Collection statistics */
export interface CollectionStats {
  /** Documents in memtable (not yet flushed) */
  memtableDocs: number;
  /** Number of segment files */
  segmentCount: number;
  /** Total documents across all segments */
  totalSegmentDocs: number;
}

/**
 * FilterBuilder for constructing Mongo-like query filters
 *
 * All methods return JSON filter strings that can be passed to search().
 */
export class FilterBuilder {
  /**
   * Equality filter: field == value
   * @param field - Field name (supports dot notation for nested fields)
   * @param value - Value to compare
   */
  static eq(field: string, value: any): string;

  /**
   * Greater than filter: field > value
   * @param field - Field name
   * @param value - Value to compare
   */
  static gt(field: string, value: any): string;

  /**
   * Greater than or equal filter: field >= value
   * @param field - Field name
   * @param value - Value to compare
   */
  static gte(field: string, value: any): string;

  /**
   * Less than filter: field < value
   * @param field - Field name
   * @param value - Value to compare
   */
  static lt(field: string, value: any): string;

  /**
   * Less than or equal filter: field <= value
   * @param field - Field name
   * @param value - Value to compare
   */
  static lte(field: string, value: any): string;

  /**
   * Not equal filter: field != value
   * @param field - Field name
   * @param value - Value to compare
   */
  static ne(field: string, value: any): string;

  /**
   * In array filter: field IN values
   * @param field - Field name
   * @param values - Array of values
   */
  static in(field: string, values: any[]): string;

  /**
   * Logical AND: all filters must match
   * @param filters - Array of filter JSON strings
   */
  static and(filters: string[]): string;

  /**
   * Logical OR: any filter must match
   * @param filters - Array of filter JSON strings
   */
  static or(filters: string[]): string;
}
