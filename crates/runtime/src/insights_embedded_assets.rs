//! Compile-time asset list for the Desktop Insights Bun worker.
//!
//! Every file is embedded in the Runtime binary and materialized into an
//! owned private temporary directory before Bun starts. There is no runtime
//! dependency on the source checkout or an Electron asar path.

#[derive(Clone, Copy)]
pub struct EmbeddedWorkerAsset {
    pub path: &'static str,
    pub bytes: &'static [u8],
}

pub const INSIGHTS_WORKER_ASSETS: &[EmbeddedWorkerAsset] = &[
    EmbeddedWorkerAsset {
        path: "insights_html_native_escape.mjs",
        bytes: include_bytes!("insights_assets/insights_html_native_escape.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_html_worker.mjs",
        bytes: include_bytes!("insights_assets/insights_html_worker.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_native_aggregate.mjs",
        bytes: include_bytes!("insights_assets/insights_native_aggregate.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_native_at_glance.mjs",
        bytes: include_bytes!("insights_assets/insights_native_at_glance.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_report_section_prompts.mjs",
        bytes: include_bytes!("insights_assets/insights_report_section_prompts.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_source_actions.mjs",
        bytes: include_bytes!("insights_assets/insights_source_actions.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_worker.mjs",
        bytes: include_bytes!("insights_assets/insights_worker.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "insights_worker_protocol.mjs",
        bytes: include_bytes!("insights_assets/insights_worker_protocol.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/insights-native-actions.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/insights-native-actions.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-command-projection/native-command-name-projection.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-command-projection/native-command-name-projection.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186014760.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186014760.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186016069.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186016069.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186023567.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186023567.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186050226.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186050226.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186056234.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186056234.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186066580.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186066580.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186068928.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186068928.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186092222.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186092222.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186094963.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186094963.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186096608.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186096608.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186220896.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186220896.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186226122.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186226122.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186231195.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186231195.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186238496.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186238496.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186240709.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186240709.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186320868.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186320868.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186325155.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186325155.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186332575.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186332575.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186336073.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186336073.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186440090.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186440090.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186441656.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186441656.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186447803.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186447803.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186448966.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186448966.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186453827.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186453827.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186455978.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186455978.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186457469.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186457469.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186460102.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186460102.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186477637.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186477637.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186481843.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186481843.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186559853.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186559853.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186562918.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186562918.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186563900.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186563900.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186567271.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186567271.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186578912.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186578912.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186669406.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186669406.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186713885.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186713885.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186715191.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186715191.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186758110.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186758110.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186758878.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186758878.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186760168.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186760168.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186794983.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186794983.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186804723.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186804723.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186854169.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186854169.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186893596.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186893596.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186909762.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186909762.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186925971.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186925971.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_186935115.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_186935115.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187591836.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187591836.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187599130.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187599130.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187600574.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187600574.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187601633.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187601633.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187620677.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187620677.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187623132.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187623132.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187644646.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187644646.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187646551.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187646551.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187649719.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187649719.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187659079.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187659079.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187665336.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187665336.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187666694.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187666694.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187667571.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187667571.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187668732.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187668732.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187670201.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187670201.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187672238.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187672238.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_187673145.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_187673145.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188108105.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188108105.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188110630.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188110630.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188115875.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188115875.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188118649.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188118649.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188142797.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188142797.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188159463.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188159463.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188182404.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188182404.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188187055.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188187055.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188283656.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188283656.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188290059.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188290059.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188299680.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188299680.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188304227.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188304227.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188305035.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188305035.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188306029.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188306029.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188419429.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188419429.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188442564.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188442564.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188445685.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188445685.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188467710.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188467710.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188469437.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188469437.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188476195.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188476195.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188478200.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188478200.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188479944.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188479944.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188480784.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188480784.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188489849.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188489849.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188508318.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188508318.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188508971.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188508971.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188539955.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188539955.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188544344.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188544344.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188544988.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188544988.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188602688.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188602688.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188615799.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188615799.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188626740.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188626740.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_188693222.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_188693222.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_189955992.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_189955992.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190006194.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190006194.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190057504.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190057504.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190060576.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190060576.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190586895.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190586895.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190593433.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190593433.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190630638.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190630638.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190631345.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190631345.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190756205.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190756205.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190805779.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190805779.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190860966.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190860966.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190865036.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190865036.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190869821.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190869821.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190872709.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190872709.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_190934551.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_190934551.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191056520.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191056520.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191305605.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191305605.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191327423.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191327423.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191336457.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191336457.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191357338.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191357338.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191360798.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191360798.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_191362209.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_191362209.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_192488660.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_192488660.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_192889237.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_192889237.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_192962497.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_192962497.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_192995347.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_192995347.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_193940785.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_193940785.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_194424922.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_194424922.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_194435847.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_194435847.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_194447854.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_194447854.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_206963702.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_206963702.mjs"),
    },
    EmbeddedWorkerAsset {
        path: "metadata-source/native-bundle/src_207151635.mjs",
        bytes: include_bytes!("insights_assets/metadata-source/native-bundle/src_207151635.mjs"),
    },
];
