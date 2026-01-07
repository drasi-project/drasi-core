DECLARE @from_lsn binary(10), @to_lsn binary(10);
SET @from_lsn = sys.fn_cdc_get_min_lsn('dbo_Orders');
SET @to_lsn = sys.fn_cdc_get_max_lsn();

SELECT 
    __$start_lsn,
    __$operation,
    __$seqval,
    OrderId,
    CustomerName,
    Status,
    TotalAmount
FROM cdc.fn_cdc_get_all_changes_dbo_Orders(@from_lsn, @to_lsn, 'all')
ORDER BY __$start_lsn, __$seqval;
