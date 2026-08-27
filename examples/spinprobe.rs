//! spinner 进度探针:旋转量/所需圈数/progress/RPM。
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let game = osu_replay_render::game::load(&args[1], &args[2]).unwrap();
    // spinner 事件结果
    for e in &game.events {
        if (e.position[0] - 256.0).abs() < 0.01 && matches!(e.result, osu_replay_judge::score::HitResult::Great | osu_replay_judge::score::HitResult::Ok | osu_replay_judge::score::HitResult::Meh | osu_replay_judge::score::HitResult::Miss) {
            if e.time > 209_000.0 && e.time < 212_000.0 {
                println!("spinner judgement @ {:.0}: {:?} display={:?}", e.time, e.result, e.display as i32);
            }
        }
    }
    // 快照里的旋转
    for s in game.snapshots.iter().filter(|s| s.time > 210_100.0 && s.time < 211_400.0).step_by(10) {
        for sp in &s.spinners {
            println!("t={:.0} rot={:.0}° turns={:.2} tracking={}", s.time, sp.total_rotation, sp.total_rotation / 360.0, sp.tracking as i32);
        }
    }
}
