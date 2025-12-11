-- ============================================================================
-- Sensor Data Schema for StoredProc Reaction Testing
-- ============================================================================
-- This schema creates tables and stored procedures to test the StoredProc
-- reaction with sensor data from a mock source.
--
-- Usage:
--   psql -h localhost -U postgres -d drasi_test -f sensor_schema.sql
--
-- ============================================================================

-- Drop existing objects if they exist
DROP TABLE IF EXISTS sensor_readings CASCADE;
DROP TABLE IF EXISTS sensor_events_log CASCADE;

-- ============================================================================
-- Target Table: sensor_readings
-- ============================================================================
-- This table stores the current state of sensor readings
CREATE TABLE sensor_readings (
    id SERIAL PRIMARY KEY,
    sensor_id VARCHAR(50) NOT NULL,
    temperature DOUBLE PRECISION,
    humidity DOUBLE PRECISION,
    timestamp TIMESTAMP WITH TIME ZONE NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    UNIQUE(sensor_id, timestamp)
);

-- Index for faster lookups
CREATE INDEX idx_sensor_readings_sensor_id ON sensor_readings(sensor_id);
CREATE INDEX idx_sensor_readings_timestamp ON sensor_readings(timestamp DESC);

-- ============================================================================
-- Event Log Table: sensor_events_log
-- ============================================================================
-- This table logs all sensor events (ADD, UPDATE, DELETE) for auditing
CREATE TABLE sensor_events_log (
    id SERIAL PRIMARY KEY,
    event_type VARCHAR(20) NOT NULL,  -- 'ADD', 'UPDATE', 'DELETE'
    sensor_id VARCHAR(50) NOT NULL,
    temperature DOUBLE PRECISION,
    humidity DOUBLE PRECISION,
    reading_timestamp TIMESTAMP WITH TIME ZONE,
    logged_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    notes TEXT
);

CREATE INDEX idx_sensor_events_log_sensor_id ON sensor_events_log(sensor_id);
CREATE INDEX idx_sensor_events_log_logged_at ON sensor_events_log(logged_at DESC);

-- ============================================================================
-- Stored Procedure: add_sensor_reading
-- ============================================================================
-- Called when a new sensor reading is added (INSERT operation)
--
-- Parameters match the query result fields:
--   - sensor_id: String from query result
--   - temperature: Float from query result
--   - humidity: Float from query result
--   - timestamp: String (ISO timestamp) from query result
--
CREATE OR REPLACE PROCEDURE add_sensor_reading(
    p_sensor_id VARCHAR,
    p_temperature DOUBLE PRECISION,
    p_humidity DOUBLE PRECISION,
    p_timestamp VARCHAR  -- Will be converted from string to timestamp
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_timestamp TIMESTAMP WITH TIME ZONE;
BEGIN
    -- Convert string timestamp to TIMESTAMPTZ
    v_timestamp := p_timestamp::TIMESTAMPTZ;

    -- Insert new sensor reading
    INSERT INTO sensor_readings (sensor_id, temperature, humidity, timestamp)
    VALUES (p_sensor_id, p_temperature, p_humidity, v_timestamp)
    ON CONFLICT (sensor_id, timestamp) DO NOTHING;  -- Avoid duplicates

    -- Log the event
    INSERT INTO sensor_events_log (
        event_type, sensor_id, temperature, humidity,
        reading_timestamp, notes
    )
    VALUES (
        'ADD', p_sensor_id, p_temperature, p_humidity,
        v_timestamp,
        format('New reading: %.2f°C, %.2f%% humidity', p_temperature, p_humidity)
    );

    RAISE NOTICE 'Added sensor reading: % - %.2f°C, %.2f%% humidity at %',
        p_sensor_id, p_temperature, p_humidity, v_timestamp;
END;
$$;

-- ============================================================================
-- Stored Procedure: update_sensor_reading
-- ============================================================================
-- Called when a sensor reading is updated (UPDATE operation)
--
CREATE OR REPLACE PROCEDURE update_sensor_reading(
    p_sensor_id VARCHAR,
    p_temperature DOUBLE PRECISION,
    p_humidity DOUBLE PRECISION,
    p_timestamp VARCHAR
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_timestamp TIMESTAMP WITH TIME ZONE;
    v_old_temp DOUBLE PRECISION;
    v_old_humidity DOUBLE PRECISION;
BEGIN
    v_timestamp := p_timestamp::TIMESTAMPTZ;

    -- Get old values for logging
    SELECT temperature, humidity INTO v_old_temp, v_old_humidity
    FROM sensor_readings
    WHERE sensor_id = p_sensor_id AND timestamp = v_timestamp;

    -- Update the reading
    UPDATE sensor_readings
    SET
        temperature = p_temperature,
        humidity = p_humidity,
        updated_at = NOW()
    WHERE sensor_id = p_sensor_id AND timestamp = v_timestamp;

    -- Log the event
    INSERT INTO sensor_events_log (
        event_type, sensor_id, temperature, humidity,
        reading_timestamp, notes
    )
    VALUES (
        'UPDATE', p_sensor_id, p_temperature, p_humidity,
        v_timestamp,
        format('Updated: %.2f°C→%.2f°C, %.2f%%→%.2f%%',
               v_old_temp, p_temperature, v_old_humidity, p_humidity)
    );

    RAISE NOTICE 'Updated sensor reading: % - %.2f°C (was %.2f°C)',
        p_sensor_id, p_temperature, v_old_temp;
END;
$$;

-- ============================================================================
-- Stored Procedure: delete_sensor_reading
-- ============================================================================
-- Called when a sensor reading is deleted (DELETE operation)
--
CREATE OR REPLACE PROCEDURE delete_sensor_reading(
    p_sensor_id VARCHAR,
    p_timestamp VARCHAR
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_timestamp TIMESTAMP WITH TIME ZONE;
    v_temp DOUBLE PRECISION;
    v_humidity DOUBLE PRECISION;
BEGIN
    v_timestamp := p_timestamp::TIMESTAMPTZ;

    -- Get values before deletion for logging
    SELECT temperature, humidity INTO v_temp, v_humidity
    FROM sensor_readings
    WHERE sensor_id = p_sensor_id AND timestamp = v_timestamp;

    -- Delete the reading
    DELETE FROM sensor_readings
    WHERE sensor_id = p_sensor_id AND timestamp = v_timestamp;

    -- Log the event
    INSERT INTO sensor_events_log (
        event_type, sensor_id, temperature, humidity,
        reading_timestamp, notes
    )
    VALUES (
        'DELETE', p_sensor_id, v_temp, v_humidity,
        v_timestamp,
        format('Deleted reading: %.2f°C, %.2f%% humidity', v_temp, v_humidity)
    );

    RAISE NOTICE 'Deleted sensor reading: % at %', p_sensor_id, v_timestamp;
END;
$$;

-- ============================================================================
-- Helper Views
-- ============================================================================

-- View: Latest readings per sensor
CREATE OR REPLACE VIEW latest_sensor_readings AS
SELECT DISTINCT ON (sensor_id)
    sensor_id,
    temperature,
    humidity,
    timestamp,
    created_at
FROM sensor_readings
ORDER BY sensor_id, timestamp DESC;

-- View: Recent events (last 100)
CREATE OR REPLACE VIEW recent_sensor_events AS
SELECT *
FROM sensor_events_log
ORDER BY logged_at DESC
LIMIT 100;

-- ============================================================================
-- Test Data (Optional)
-- ============================================================================
-- Uncomment to insert sample data for testing

-- INSERT INTO sensor_readings (sensor_id, temperature, humidity, timestamp)
-- VALUES
--     ('sensor_1', 22.5, 55.0, NOW() - INTERVAL '5 minutes'),
--     ('sensor_2', 23.1, 58.2, NOW() - INTERVAL '4 minutes'),
--     ('sensor_3', 24.7, 61.5, NOW() - INTERVAL '3 minutes');

-- ============================================================================
-- Verification Queries
-- ============================================================================

-- Check tables exist
SELECT 'Tables created:' as status;
SELECT tablename FROM pg_tables
WHERE schemaname = 'public'
AND tablename IN ('sensor_readings', 'sensor_events_log');

-- Check procedures exist
SELECT 'Stored procedures created:' as status;
SELECT proname, prokind FROM pg_proc
WHERE proname IN ('add_sensor_reading', 'update_sensor_reading', 'delete_sensor_reading');

-- Show table structures
\d sensor_readings
\d sensor_events_log

-- Show procedure signatures
\df add_sensor_reading
\df update_sensor_reading
\df delete_sensor_reading

NOTIFY setup_complete, 'Sensor schema setup completed successfully!';
