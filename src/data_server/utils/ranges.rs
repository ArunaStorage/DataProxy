use anyhow::Result;
use aruna_file::helpers::footer_parser::{FooterParser, Range as ArunaRange};
use s3s::dto::Range as S3Range;
use s3s::dto::Range::{Int, Suffix};

pub fn calculate_ranges(
    input_range: Option<S3Range>,
    content_length: u64,
    footer: Option<FooterParser>,
) -> Result<(Option<String>, Option<ArunaRange>)> {
    match input_range {
        Some(r) => match footer {
            Some(mut foot) => {
                foot.parse()?;
                let (o1, mut o2) =
                    foot.get_offsets_by_range(aruna_range_from_s3range(r, content_length))?;
                o2.to += 1;
                Ok((Some(format!("bytes={}-{}", o1.from, o1.to - 1)), Some(o2)))
            }
            None => {
                let mut ar_range = aruna_range_from_s3range(r, content_length);
                ar_range.to += 1;
                Ok((
                    None,
                    Some(ar_range), //Some(format!("bytes={}-{}", ar_range.from, ar_range.to)),
                ))
            }
        },
        None => Ok((None, None)),
    }
}

pub fn calculate_content_length_from_range(range: ArunaRange) -> i64 {
    (range.to - range.from) as i64 // Note: -1 bytes-ranges are inclusive
}

pub fn aruna_range_from_s3range(range_string: S3Range, content_length: u64) -> ArunaRange {
    match range_string {
        Int { first, last } => match last {
            Some(val) => ArunaRange {
                from: first,
                to: val,
            },
            None => ArunaRange {
                from: first,
                to: content_length,
            },
        },
        Suffix { length } => ArunaRange {
            from: content_length - length,
            to: content_length,
        },
    }
}
