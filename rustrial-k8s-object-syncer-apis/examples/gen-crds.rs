use kube::CustomResourceExt;
use rustrial_k8s_object_syncer_apis::ObjectSync;

pub fn main() {
    println!("{}", serde_saphyr::to_string(&ObjectSync::crd()).unwrap());
}
