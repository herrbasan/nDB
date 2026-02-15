const { Database, FilterBuilder } = require('../index.js');

// Create/open database
const db = new Database('./example-data');

// Create a collection for 4-dimensional vectors (small for demo)
let collection;
try {
  collection = db.createCollection('items', 4, { durability: 'sync' });
  console.log('Collection created:', collection.name);
} catch (e) {
  collection = db.getCollection('items');
  console.log('Collection opened:', collection.name);
}

console.log('Config:', collection.config);

// Insert some documents
console.log('\nInserting documents...');
collection.insert('item1', [1.0, 0.0, 0.0, 0.0], JSON.stringify({ category: 'electronics', price: 100 }));
collection.insert('item2', [0.0, 1.0, 0.0, 0.0], JSON.stringify({ category: 'clothing', price: 50 }));
collection.insert('item3', [0.0, 0.0, 1.0, 0.0], JSON.stringify({ category: 'food', price: 10 }));
collection.insert('item4', [0.9, 0.1, 0.0, 0.0], JSON.stringify({ category: 'electronics', price: 150 }));

// Batch insert example
console.log('Batch inserting...');
collection.insertBatch([
  { id: 'item5', vector: [0.5, 0.5, 0.0, 0.0], payload: JSON.stringify({ category: 'mixed', price: 75 }) },
  { id: 'item6', vector: [0.0, 0.0, 0.0, 1.0], payload: JSON.stringify({ category: 'other', price: 25 }) },
]);

// Retrieve by ID
console.log('\nGetting item1:');
const item1 = collection.get('item1');
console.log(item1);

// Search EXACT (works with memtable data)
console.log('\n=== EXACT SEARCH (works immediately) ===');
console.log('Searching for vectors similar to [1.0, 0.0, 0.0, 0.0]:');
const exactResults = collection.search({
  vector: [1.0, 0.0, 0.0, 0.0],
  topK: 3,
  distance: 'cosine',
  approximate: false
});

exactResults.forEach((match, i) => {
  console.log(`  ${i + 1}. ${match.id}: score=${match.score.toFixed(4)}`);
});

// Search with filter (exact search with filtering)
console.log('\n=== SEARCH WITH FILTER ===');
const filter = FilterBuilder.eq('category', 'electronics');
console.log('Filter JSON:', filter);

const filteredResults = collection.search({
  vector: [1.0, 0.0, 0.0, 0.0],
  topK: 3,
  distance: 'cosine',
  approximate: false,
  filter: filter
});

filteredResults.forEach((match, i) => {
  console.log(`  ${i + 1}. ${match.id}: score=${match.score.toFixed(4)}`);
  const payload = JSON.parse(match.payload);
  console.log(`     category: ${payload.category}, price: $${payload.price}`);
});

// Flush data to segments before building HNSW index
console.log('\nFlushing to disk before building index...');
collection.flush();
console.log('Stats after flush:', collection.stats);

// Build HNSW index for approximate search
console.log('\nBuilding HNSW index (from segment data)...');
collection.rebuildIndex();
console.log('Index exists:', collection.hasIndex());

// Search APPROXIMATE (uses HNSW - only works on flushed/segment data)
console.log('\n=== APPROXIMATE SEARCH (HNSW) ===');
console.log('Searching for vectors similar to [1.0, 0.0, 0.0, 0.0]:');
const approxResults = collection.search({
  vector: [1.0, 0.0, 0.0, 0.0],
  topK: 3,
  distance: 'cosine',
  approximate: true,
  ef: 64
});

if (approxResults.length === 0) {
  console.log('  (no results - index only covers flushed data)');
} else {
  approxResults.forEach((match, i) => {
    console.log(`  ${i + 1}. ${match.id}: score=${match.score.toFixed(4)}`);
  });
}

// Add more data and show the difference
console.log('\n=== ADDING MORE DATA ===');
collection.insert('item7', [0.95, 0.05, 0.0, 0.0], JSON.stringify({ category: 'electronics', price: 200 }));
console.log('Added item7 (in memtable only)');

console.log('\nExact search finds item7:');
const exactAfter = collection.search({
  vector: [1.0, 0.0, 0.0, 0.0],
  topK: 3,
  distance: 'cosine',
  approximate: false
});
exactAfter.forEach((match, i) => {
  console.log(`  ${i + 1}. ${match.id}: score=${match.score.toFixed(4)}`);
});

console.log('\nApproximate search does NOT find item7 (not in index):');
const approxAfter = collection.search({
  vector: [1.0, 0.0, 0.0, 0.0],
  topK: 3,
  distance: 'cosine',
  approximate: true,
  ef: 64
});
approxAfter.forEach((match, i) => {
  console.log(`  ${i + 1}. ${match.id}: score=${match.score.toFixed(4)}`);
});

console.log('\nFinal stats:', collection.stats);
console.log('\nDone!');
