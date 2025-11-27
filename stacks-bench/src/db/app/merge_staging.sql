-- 1. Merge Burn Blocks
INSERT INTO burn_block (block_hash, height)
SELECT DISTINCT burn_block_hash, burn_block_height FROM _staged_stacks_block
WHERE true
ON CONFLICT (block_hash) DO NOTHING;

-- 2. Merge Stacks Blocks (Initial insert with NULL parent)
INSERT INTO stacks_block (index_hash, height, burn_block_id, parent_stacks_block_id)
SELECT 
    sb.index_hash, 
    sb.height, 
    bb.id, 
    NULL
FROM _staged_stacks_block sb
JOIN burn_block bb ON sb.burn_block_hash = bb.block_hash
WHERE true
ON CONFLICT (index_hash) DO NOTHING;

-- 3. Link Parents (SQLite UPDATE FROM syntax)
UPDATE stacks_block
SET parent_stacks_block_id = parent.id
FROM _staged_stacks_block stage, stacks_block parent
WHERE stacks_block.index_hash = stage.index_hash
  AND parent.index_hash = stage.parent_index_hash;

-- 4. Merge Transactions
INSERT INTO stacks_tx (stacks_block_id, tx_hash, tx_type)
SELECT 
    b.id,
    st.tx_hash,
    st.tx_type
FROM _staged_stacks_tx st
JOIN stacks_block b ON st.block_index_hash = b.index_hash
WHERE true
ON CONFLICT (stacks_block_id, tx_hash) DO NOTHING;